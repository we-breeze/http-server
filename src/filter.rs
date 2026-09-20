use crate::{ApiMetrics, Handler, NoAuthenticator, Request, Response, Router};

/// Decision returned by [`HttpFilter::before`].
#[derive(Debug)]
pub enum FilterDecision {
    /// Continue to the wrapped handler.
    Continue,
    /// Return a response without invoking the wrapped handler.
    Respond(Box<Response>),
}

impl FilterDecision {
    /// Short-circuits the wrapped handler with `response`.
    #[must_use]
    pub fn respond(response: Response) -> Self {
        Self::Respond(Box::new(response))
    }
}

/// A synchronous request/response filter that is independent of route business logic.
///
/// Filters can reject a request before parameter extraction and can transform the
/// resulting response. Compose filters by calling [`HandlerExt::with_filter`]
/// repeatedly; request filters run from outermost to innermost and response
/// filters run in reverse order.
pub trait HttpFilter: Send + Sync + 'static {
    /// Inspect a request before the wrapped handler runs.
    fn before(&self, _request: &Request<'_>) -> FilterDecision {
        FilterDecision::Continue
    }

    /// Transform the wrapped handler's response.
    fn after(&self, _request: &Request<'_>, response: Response) -> Response {
        response
    }
}

/// A handler decorated with one [`HttpFilter`].
pub struct Filtered<H, F> {
    inner: H,
    filter: F,
}

impl<H, F> Filtered<H, F> {
    #[must_use]
    pub const fn new(inner: H, filter: F) -> Self {
        Self { inner, filter }
    }

    #[must_use]
    pub fn into_inner(self) -> H {
        self.inner
    }

    #[must_use]
    pub const fn filter(&self) -> &F {
        &self.filter
    }
}

/// Extension methods for decorating a handler with generic HTTP filters.
pub trait HandlerExt<A = NoAuthenticator>: Handler<A> + Sized
where
    A: Send + Sync + 'static,
{
    #[must_use]
    fn with_filter<F>(self, filter: F) -> Filtered<Self, F>
    where
        F: HttpFilter,
    {
        Filtered::new(self, filter)
    }
}

impl<H, A> HandlerExt<A> for H
where
    H: Handler<A>,
    A: Send + Sync + 'static,
{
}

impl<H, F, A> Handler<A> for Filtered<H, F>
where
    H: Handler<A>,
    F: HttpFilter,
    A: Send + Sync + 'static,
{
    fn register_metrics(&self) {
        self.inner.register_metrics();
    }

    fn route_metrics(&self, path: &str, method: &str) -> Option<(usize, ApiMetrics)> {
        self.inner.route_metrics(path, method)
    }

    fn route_priority(&self, path: &str, method: &str) -> Option<usize> {
        self.inner.route_priority(path, method)
    }

    fn route_methods(&self, path: &str) -> u16 {
        self.inner.route_methods(path)
    }

    fn routes(&self) -> &'static [crate::__private::RouteDescriptor] {
        self.inner.routes()
    }

    fn append_to(self, router: &mut Router<A>) {
        router.push(self);
    }

    fn prepare<'p>(&self, path: &'p str, method: &str) -> crate::__private::PreparedRoute<'p> {
        self.inner.prepare(path, method)
    }

    async fn call_prepared<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
        prepared: &'a crate::__private::PreparedRoute<'_>,
    ) -> Response {
        match self.filter.before(&request) {
            FilterDecision::Continue => {
                let response = self
                    .inner
                    .call_prepared(request, authenticator, prepared)
                    .await;
                self.filter.after(&request, response)
            }
            FilterDecision::Respond(response) => self.filter.after(&request, *response),
        }
    }

    async fn call_route<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
        route: usize,
        captures: crate::__private::RouteMatch<'a>,
    ) -> Response {
        match self.filter.before(&request) {
            FilterDecision::Continue => {
                let response = self
                    .inner
                    .call_route(request, authenticator, route, captures)
                    .await;
                self.filter.after(&request, response)
            }
            FilterDecision::Respond(response) => self.filter.after(&request, *response),
        }
    }

    async fn call<'a>(&'a self, request: Request<'a>, authenticator: &'a A) -> Response {
        match self.filter.before(&request) {
            FilterDecision::Continue => {
                let response = self.inner.call(request, authenticator).await;
                self.filter.after(&request, response)
            }
            FilterDecision::Respond(response) => self.filter.after(&request, *response),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{EphemeralBytesArena, Header, NoAuthenticator, ResponseBody, StatusCode};

    struct Echo {
        calls: AtomicUsize,
    }

    impl Handler for Echo {
        fn call(
            &self,
            _request: Request<'_>,
            _authenticator: &NoAuthenticator,
        ) -> impl Future<Output = Response> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Response::static_bytes(StatusCode::OK, b"ok"))
        }
    }

    struct Guard;

    impl HttpFilter for Guard {
        fn before(&self, request: &Request<'_>) -> FilterDecision {
            if request.header("x-deny").is_some() {
                FilterDecision::respond(Response::empty(StatusCode::FORBIDDEN))
            } else {
                FilterDecision::Continue
            }
        }

        fn after(&self, _request: &Request<'_>, response: Response) -> Response {
            if response.status() == StatusCode::OK {
                response.with_status(StatusCode::CREATED)
            } else {
                response
            }
        }
    }

    fn request<'a>(arena: &'a EphemeralBytesArena, headers: &'a [Header<'a>]) -> Request<'a> {
        let body = Box::leak(Box::new(brz_io::Writer::new(arena).into_reader()));
        Request::new(
            "GET",
            "/",
            headers,
            body,
            "127.0.0.1:1".parse().unwrap(),
            arena,
        )
    }

    #[tokio::test]
    async fn continues_and_filters_the_response() {
        let arena = EphemeralBytesArena::new(128);
        let handler = Echo {
            calls: AtomicUsize::new(0),
        }
        .with_filter(Guard);

        let response = handler.call(request(&arena, &[]), &NoAuthenticator).await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(matches!(response.body(), ResponseBody::Static(body) if *body == b"ok"));
        assert_eq!(handler.inner.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn short_circuits_the_wrapped_handler() {
        let arena = EphemeralBytesArena::new(128);
        let headers = [Header {
            name: "x-deny",
            value: b"1",
        }];
        let handler = Echo {
            calls: AtomicUsize::new(0),
        }
        .with_filter(Guard);

        let response = handler
            .call(request(&arena, &headers), &NoAuthenticator)
            .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(handler.inner.calls.load(Ordering::Relaxed), 0);
    }
}
