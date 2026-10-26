//! Optional Hyper transport for protocol services on the same HTTP/1 listener.
//!
//! [`HandlerService`] adapts the normal Breeze router and its exported HTTP
//! handlers. [`Mount`] selects a protocol service before that adapter, so long
//! polling and upgraded sessions do not inherit the short HTTP handler timeout.
//! The native [`crate::Server`] remains available without this feature.
mod native;
mod runtime;
mod tasks;
use ::hyper::{
    body::{Body, Incoming},
    service::Service,
};
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, combinators::UnsyncBoxBody};
pub use native::HandlerService;
pub use runtime::{Config, PeerAddr, serve};
use std::{convert::Infallible, future::Future};
#[doc(hidden)]
pub use tasks::UpgradeTasks;

/// Owned service/transport error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// Streaming HTTP response body used by protocol mounts.
pub type ResponseBody = UnsyncBoxBody<Bytes, BoxError>;
/// Streaming HTTP response used by protocol mounts.
pub type HttpResponse = Response<ResponseBody>;

/// Mount one protocol service at an exact HTTP path (with optional trailing
/// slash), falling back to another service for ordinary HTTP routes.
#[derive(Clone)]
pub struct Mount<S, F> {
    path: String,
    service: S,
    fallback: F,
    cors: Option<std::sync::Arc<crate::Cors>>,
}
impl<S, F> Mount<S, F> {
    /// Build a mount. Namespace names belong inside the protocol service and
    /// are independent of this path. Prefix lookalikes never match.
    pub fn new(path: impl Into<String>, service: S, fallback: F) -> Result<Self, crate::Error> {
        let path = path.into();
        if !path.starts_with('/')
            || path == "/"
            || path.ends_with('/')
            || path.bytes().any(|b| {
                b.is_ascii_whitespace() || b.is_ascii_control() || matches!(b, b'?' | b'#')
            })
        {
            return Err(crate::Error::InvalidConfig("invalid protocol mount path"));
        }
        Ok(Self {
            path,
            service,
            fallback,
            cors: None,
        })
    }
    /// Apply one outer CORS policy to protocol and ordinary HTTP responses.
    /// Preflights run before dispatch; response-local opt-outs are respected.
    pub fn with_cors(mut self, cors: crate::Cors) -> Result<Self, crate::Error> {
        if !cors.validate() {
            return Err(crate::Error::InvalidConfig("invalid CORS policy"));
        }
        self.cors = Some(std::sync::Arc::new(cors));
        Ok(self)
    }
}
impl<S, F, B, C> Service<Request<Incoming>> for Mount<S, F>
where
    S: Service<Request<Incoming>, Response = Response<B>, Error = Infallible> + Sync,
    F: Service<Request<Incoming>, Response = Response<C>, Error = Infallible> + Sync,
    S::Future: Send + 'static,
    F::Future: Send + 'static,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
    C: Body<Data = Bytes> + Send + 'static,
    C::Error: Into<BoxError>,
{
    type Response = HttpResponse;
    type Error = Infallible;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<HttpResponse, Infallible>> + Send>>;
    fn call(&self, request: Request<Incoming>) -> Self::Future {
        let cors = self.cors.clone();
        if let Some(preflight) = cors.as_ref().and_then(|cors| {
            cors.preflight(brz_http_cors::PreflightRequest {
                method: request.method().as_str(),
                origin: request
                    .headers()
                    .get("origin")
                    .map(http::HeaderValue::as_bytes),
                request_method: request
                    .headers()
                    .get("access-control-request-method")
                    .map(http::HeaderValue::as_bytes),
                request_headers: request
                    .headers()
                    .get("access-control-request-headers")
                    .map(http::HeaderValue::as_bytes),
            })
        }) {
            let mut response = Response::new(
                http_body_util::Full::new(Bytes::from_static(preflight.body))
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            );
            *response.status_mut() = preflight.status;
            *response.headers_mut() = preflight.headers;
            return Box::pin(std::future::ready(Ok(response)));
        }
        let origin = request.headers().get("origin").cloned();
        let path = request.uri().path();
        let future = if path == self.path || path.strip_suffix('/') == Some(self.path.as_str()) {
            let future = self.service.call(request);
            futures_util::future::Either::Left(async move {
                future
                    .await
                    .map(|r| r.map(|b| b.map_err(Into::into).boxed_unsync()))
            })
        } else {
            let future = self.fallback.call(request);
            futures_util::future::Either::Right(async move {
                future
                    .await
                    .map(|r| r.map(|b| b.map_err(Into::into).boxed_unsync()))
            })
        };
        Box::pin(async move {
            let mut response = future.await?;
            if let Some(cors) = cors {
                cors.apply_response(
                    origin.as_ref().map(http::HeaderValue::as_bytes),
                    &mut response,
                );
            }
            Ok(response)
        })
    }
}

/// Upstream/downstream negotiation handles. Capturing does not accept an
/// upgrade; only a successful HTTP 101 response transfers the connection.
pub use ::hyper::upgrade::OnUpgrade as PendingUpgrade;
/// Capture a request's pending protocol handoff.
pub fn request_upgrade<B>(request: &mut Request<B>) -> PendingUpgrade {
    ::hyper::upgrade::on(request)
}
/// Capture a successful upstream response's pending protocol handoff.
pub fn response_upgrade<B>(response: &mut Response<B>) -> PendingUpgrade {
    ::hyper::upgrade::on(response)
}
/// Bridge a successfully upgraded connection pair without decoding frames.
/// Both halves retain the listener's connection lifetime and shutdown policy.
pub async fn relay(downstream: PendingUpgrade, upstream: PendingUpgrade) -> std::io::Result<()> {
    let (downstream, upstream) =
        tokio::try_join!(downstream, upstream).map_err(std::io::Error::other)?;
    let mut downstream = hyper_util::rt::TokioIo::new(downstream);
    let mut upstream = hyper_util::rt::TokioIo::new(upstream);
    tokio::io::copy_bidirectional(&mut downstream, &mut upstream)
        .await
        .map(|_| ())
}
