use super::{BoxError, HttpResponse, PeerAddr, PendingUpgrade};
use crate::{Handler, Header, NoAuthenticator, Request, Response, ResponseBody, ServerConfig};
use ::hyper::{
    body::{Body, Incoming},
    service::Service,
};
use bytes::Bytes;
use http::{Request as HttpRequest, Response as HyperResponse, StatusCode};
use http_body_util::{BodyExt, Full};
use std::{convert::Infallible, future::Future, io, pin::Pin, sync::Arc};
use tokio::sync::Semaphore;

/// Adapts native handlers (including macro-exported routers) to a shared Hyper
/// listener. Only requests dispatched here receive the short HTTP timeout and
/// buffered body bounds. Protocol mounts bypass this adapter.
pub struct HandlerService<H, A = NoAuthenticator> {
    handler: Arc<H>,
    auth: Arc<A>,
    config: ServerConfig,
    body_limit: Arc<Semaphore>,
}
impl<H, A> Clone for HandlerService<H, A> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
            auth: self.auth.clone(),
            config: self.config.clone(),
            body_limit: self.body_limit.clone(),
        }
    }
}
impl<H: Handler> HandlerService<H> {
    /// Adapt an ordinary HTTP router with explicit HTTP limits.
    pub fn new(handler: H, config: ServerConfig) -> crate::Result<Self> {
        Self::with_authenticator(handler, NoAuthenticator, config)
    }
}
impl<H: Handler<A>, A: Send + Sync + 'static> HandlerService<H, A> {
    /// Adapt a router using its normal application authenticator.
    pub fn with_authenticator(handler: H, auth: A, config: ServerConfig) -> crate::Result<Self> {
        config.validate()?;
        let body_limit = Arc::new(Semaphore::new(config.max_in_flight_request_body_bytes));
        Ok(Self {
            handler: Arc::new(handler),
            auth: Arc::new(auth),
            config,
            body_limit,
        })
    }
    async fn handle(&self, mut request: HttpRequest<Incoming>) -> HttpResponse {
        let upgrade = super::request_upgrade(&mut request);
        let peer = request
            .extensions()
            .get::<PeerAddr>()
            .map_or_else(|| "0.0.0.0:0".parse().expect("static peer"), |p| p.0);
        let (mut parts, mut body) = request.into_parts();
        let tasks = parts.extensions.remove::<super::UpgradeTasks>();
        let budget = body
            .size_hint()
            .upper()
            .unwrap_or(self.config.max_request_body_bytes as u64);
        if budget > self.config.max_request_body_bytes as u64 {
            return convert(
                Response::empty(StatusCode::PAYLOAD_TOO_LARGE).close(),
                upgrade,
                tasks,
            );
        }
        let target = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        let path = parts.uri.path();
        let prepared = self.handler.prepare(path, parts.method.as_str());
        let mut observation = crate::api_metrics::Observation::new();
        observation.matched(prepared.metrics());
        observation.set_api_log(prepared.api_log());
        #[cfg(any(feature = "api-log", feature = "slow-log"))]
        let head = {
            let mut head = format!("{} {} HTTP/1.1\r\n", parts.method, target).into_bytes();
            for (name, value) in &parts.headers {
                head.extend_from_slice(name.as_str().as_bytes());
                head.extend_from_slice(b": ");
                head.extend_from_slice(value.as_bytes());
                head.extend_from_slice(b"\r\n");
            }
            head.extend_from_slice(b"\r\n");
            let mut headers = vec![httparse::EMPTY_HEADER; parts.headers.len()];
            let mut parsed = httparse::Request::new(&mut headers);
            if parsed.parse(&head).is_ok() {
                observation.request_head(&head, &parsed, peer, budget as usize);
            }
            head
        };
        #[cfg(not(any(feature = "api-log", feature = "slow-log")))]
        let head: &[u8] = &[];
        let execute = async {
            let _permit = self
                .body_limit
                .acquire_many(budget as u32)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let mut writer = brz_io::Writer::with_initial_segment_size(
                &self.config.arena,
                self.config.initial_request_segment_bytes,
            );
            let mut received = 0usize;
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
                if let Ok(data) = frame.into_data() {
                    received = received
                        .checked_add(data.len())
                        .ok_or(StatusCode::PAYLOAD_TOO_LARGE)?;
                    if received > self.config.max_request_body_bytes {
                        return Err(StatusCode::PAYLOAD_TOO_LARGE);
                    }
                    std::io::Write::write_all(&mut writer, &data)
                        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                }
            }
            let reader = writer.into_reader();
            let headers: Vec<_> = parts
                .headers
                .iter()
                .map(|(name, value)| Header {
                    name: name.as_str(),
                    value: value.as_bytes(),
                })
                .collect();
            let mut request = Request::new(
                parts.method.as_str(),
                target,
                &headers,
                &reader,
                peer,
                &self.config.arena,
                #[cfg(feature = "api-log")]
                observation.api_log_context(),
            );
            request.rejection_handler = self.config.rejection_handler;
            let mut response = if let Some(response) = self
                .config
                .cors
                .as_ref()
                .and_then(|cors| crate::cors::preflight(cors, &request))
            {
                response
            } else {
                let response = self
                    .handler
                    .call_prepared(request, &self.auth, &prepared)
                    .await;
                if let Some(cors) = &self.config.cors {
                    response.apply_cors(cors, request.header("origin"), &self.config.arena)
                } else {
                    response
                }
            };
            if parts.method == http::Method::HEAD {
                response.suppress_body();
            }
            if self.config.export_cors_policy {
                response = response.export_cors_policy(&self.config.arena);
            }
            observation.record(
                response.status(),
                response.body().content_length(),
                false,
                &head,
                Some(reader.view()),
            );
            Ok(response)
        };
        let response = match tokio::time::timeout(self.config.request_timeout, execute).await {
            Ok(Ok(response)) => response,
            Ok(Err(status)) => {
                observation.record(status, Some(0), false, &head, None);
                Response::empty(status).close()
            }
            Err(_) => {
                observation.record(StatusCode::REQUEST_TIMEOUT, Some(0), true, &head, None);
                Response::empty(StatusCode::REQUEST_TIMEOUT).close()
            }
        };
        convert(response, upgrade, tasks)
    }
}
impl<H: Handler<A>, A: Send + Sync + 'static> Service<HttpRequest<Incoming>>
    for HandlerService<H, A>
{
    type Response = HttpResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, Infallible>> + Send>>;
    fn call(&self, request: HttpRequest<Incoming>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { Ok(this.handle(request).await) })
    }
}
fn convert(
    mut response: Response,
    pending: PendingUpgrade,
    tasks: Option<super::UpgradeTasks>,
) -> HttpResponse {
    if response.status().as_u16() > 599
        || (response.status().is_informational()
            && (response.status() != StatusCode::SWITCHING_PROTOCOLS || response.upgrade.is_none()))
        || (response.upgrade.is_some()
            && (response.status() != StatusCode::SWITCHING_PROTOCOLS || response.should_close()))
    {
        response = Response::empty(StatusCode::INTERNAL_SERVER_ERROR).close();
    }
    let mut builder = HyperResponse::builder()
        .status(response.status())
        .extension(response.cors_policy());
    if let Some(block) = response.header_block() {
        for line in block
            .as_slice()
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
        {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let colon = line
                .iter()
                .position(|b| *b == b':')
                .expect("validated header");
            builder = builder.header(&line[..colon], line[colon + 1..].trim_ascii());
        }
    }
    if let Some(value) = response.content_type_ref() {
        builder = builder.header(http::header::CONTENT_TYPE, value);
    }
    if let Some(value) = response.allow_ref() {
        builder = builder.header(http::header::ALLOW, value);
    }
    if let Some(value) = response.www_authenticate_ref() {
        builder = builder.header(http::header::WWW_AUTHENTICATE, value);
    }
    if response.should_close() {
        builder = builder.header(http::header::CONNECTION, "close");
    }
    if let Some(protocol) = response.upgrade_protocol() {
        builder = builder
            .header(http::header::CONNECTION, "Upgrade")
            .header(http::header::UPGRADE, protocol);
    }
    // HEAD still reports the selected representation's length.
    if response.permits_body() {
        if let Some(length) = response.body().content_length() {
            builder = builder.header(http::header::CONTENT_LENGTH, length);
        }
    }
    let sends_body = response.sends_body();
    let body = std::mem::replace(response.body_mut(), ResponseBody::Empty);
    if let Some(upgrade) = response.upgrade.take() {
        let session = async move {
            if let Ok(io) = pending.await {
                if let Err(error) = upgrade
                    .run(crate::Upgraded::new(
                        hyper_util::rt::TokioIo::new(io),
                        Bytes::new(),
                    ))
                    .await
                {
                    tracing::debug!(%error, "upgraded session closed");
                }
            }
        };
        if let Some(tasks) = tasks {
            tasks.spawn(session);
        } else {
            tokio::spawn(session);
        }
    }
    let body = if !sends_body {
        full(Bytes::new())
    } else {
        match body {
            ResponseBody::Empty => full(Bytes::new()),
            ResponseBody::Static(bytes) => full(Bytes::from_static(bytes)),
            ResponseBody::Owned(bytes) => full(bytes),
            ResponseBody::Arena(bytes) => full(Bytes::from_owner(bytes)),
            ResponseBody::Segmented(reader) => {
                let stream = futures_util::stream::try_unfold(reader, |mut reader| async move {
                    let chunk = std::io::BufRead::fill_buf(&mut reader)?;
                    if chunk.is_empty() {
                        return Ok::<_, io::Error>(None);
                    }
                    let bytes = Bytes::copy_from_slice(chunk);
                    std::io::BufRead::consume(&mut reader, bytes.len());
                    Ok(Some((::hyper::body::Frame::data(bytes), reader)))
                });
                http_body_util::StreamBody::new(stream)
                    .map_err(|e| Box::new(e) as BoxError)
                    .boxed_unsync()
            }
            ResponseBody::Stream(stream) => {
                let stream = futures_util::stream::try_unfold(stream, |mut stream| async move {
                    let chunk = stream.next().await?;
                    Ok::<_, io::Error>(
                        chunk.map(|bytes| (::hyper::body::Frame::data(bytes), stream)),
                    )
                });
                http_body_util::StreamBody::new(stream)
                    .map_err(|e| Box::new(e) as BoxError)
                    .boxed_unsync()
            }
        }
    };
    drop(response);
    builder.body(body).unwrap_or_else(|_| {
        HyperResponse::builder()
            .status(500)
            .body(full(Bytes::new()))
            .expect("static response")
    })
}
fn full(bytes: Bytes) -> super::ResponseBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}
