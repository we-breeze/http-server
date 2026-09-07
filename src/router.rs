use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use crate::route::RouteMatch;
use crate::{
    ApiMetrics, Authenticator, Handler, IntoHttpResponse, NoAuthenticator, Request, Response,
    StatusCode,
};

mod index;
pub use index::{PreparedRoute, RouteDescriptor};
use index::{RegisteredRoute, RouteIndex};

/// Composes API instances without growing the handler or future type.
///
/// Routes are indexed once before serving. Static paths use byte-length buckets;
/// parameter paths use segment-count buckets. Only the selected handler's future
/// is boxed. Each API retains its own application state.
pub struct Router<A: Authenticator = NoAuthenticator> {
    handlers: Vec<Box<dyn ErasedHandler<A>>>,
    routes: Vec<RegisteredRoute>,
    legacy: Vec<usize>,
    index: OnceLock<RouteIndex>,
}

impl<A: Authenticator> Default for Router<A> {
    fn default() -> Self {
        Self {
            handlers: Vec::new(),
            routes: Vec::new(),
            legacy: Vec::new(),
            index: OnceLock::new(),
        }
    }
}

impl<A: Authenticator> Router<A> {
    #[must_use]
    pub fn new<H: Handler<A>>(handler: H) -> Self {
        Self::default().merge(handler)
    }

    #[must_use]
    pub fn merge<H: Handler<A>>(mut self, handler: H) -> Self {
        handler.append_to(&mut self);
        self
    }

    pub(crate) fn push<H: Handler<A>>(&mut self, handler: H) {
        self.index.take();
        let id = self.handlers.len();
        let descriptors = handler.routes();
        if descriptors.is_empty() {
            self.legacy.push(id);
        }
        self.routes.extend(
            descriptors
                .iter()
                .enumerate()
                .map(|(endpoint, descriptor)| RegisteredRoute {
                    handler: id,
                    endpoint,
                    descriptor: *descriptor,
                }),
        );
        self.handlers.push(Box::new(handler));
    }

    fn index(&self) -> &RouteIndex {
        self.index.get_or_init(|| RouteIndex::new(&self.routes))
    }
}

impl<A: Authenticator> Handler<A> for Router<A> {
    fn append_to(mut self, router: &mut Self) {
        router.index.take();
        let offset = router.handlers.len();
        router.handlers.append(&mut self.handlers);
        router
            .legacy
            .extend(self.legacy.into_iter().map(|id| id + offset));
        router
            .routes
            .extend(self.routes.into_iter().map(|mut route| {
                route.handler += offset;
                route
            }));
    }

    fn register_metrics(&self) {
        let _ = self.index();
        for handler in &self.handlers {
            handler.register_metrics();
        }
    }

    fn route_metrics(&self, path: &str, method: &str) -> Option<(usize, ApiMetrics)> {
        self.prepare(path, method).metric
    }

    fn route_priority(&self, path: &str, method: &str) -> Option<usize> {
        self.prepare(path, method)
            .target
            .map(|target| target.priority)
    }

    fn route_methods(&self, path: &str) -> u16 {
        self.prepare(path, "").allowed
    }

    fn prepare<'p>(&self, path: &'p str, method: &str) -> PreparedRoute<'p> {
        let mut prepared = self.index().resolve(path, method);
        // Compatibility for hand-written handlers that provide their own route
        // metadata. Macro APIs never take this path.
        for &id in &self.legacy {
            let handler = &self.handlers[id];
            prepared.consider_legacy(
                id,
                handler.route_priority(path, method),
                handler.route_metrics(path, method),
                handler.route_methods(path),
            );
        }
        prepared
    }

    async fn call_prepared<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
        prepared: &'a PreparedRoute<'_>,
    ) -> Response {
        if let Some(target) = prepared.target {
            let route = target
                .endpoint
                .map(|endpoint| (endpoint, prepared.captures()));
            self.handlers[target.handler]
                .call(request, authenticator, route)
                .await
        } else {
            unmatched(prepared.allowed, request.response_arena())
        }
    }

    async fn call(&self, request: Request<'_>, authenticator: &A) -> Response {
        let prepared = self.prepare(request.path(), request.method());
        self.call_prepared(request, authenticator, &prepared).await
    }
}

type ResponseFuture<'a> = Pin<Box<dyn Future<Output = Response> + Send + 'a>>;

trait ErasedHandler<A: Authenticator>: Send + Sync {
    fn register_metrics(&self);
    fn route_priority(&self, path: &str, method: &str) -> Option<usize>;
    fn route_metrics(&self, path: &str, method: &str) -> Option<(usize, ApiMetrics)>;
    fn route_methods(&self, path: &str) -> u16;
    fn call<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
        route: Option<(usize, RouteMatch<'a>)>,
    ) -> ResponseFuture<'a>;
}

impl<A: Authenticator, H: Handler<A>> ErasedHandler<A> for H {
    fn register_metrics(&self) {
        Handler::register_metrics(self);
    }
    fn route_priority(&self, path: &str, method: &str) -> Option<usize> {
        Handler::route_priority(self, path, method)
    }
    fn route_metrics(&self, path: &str, method: &str) -> Option<(usize, ApiMetrics)> {
        Handler::route_metrics(self, path, method)
    }
    fn route_methods(&self, path: &str) -> u16 {
        Handler::route_methods(self, path)
    }
    fn call<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
        route: Option<(usize, RouteMatch<'a>)>,
    ) -> ResponseFuture<'a> {
        match route {
            Some((endpoint, captures)) => {
                Box::pin(self.call_route(request, authenticator, endpoint, captures))
            }
            None => Box::pin(Handler::call(self, request, authenticator)),
        }
    }
}

/// Generates the shared 404/405 response after checking all matching routes.
#[doc(hidden)]
#[must_use]
pub fn unmatched(methods: u16, arena: &crate::EphemeralBytesArena) -> Response {
    if methods == 0 {
        return Response::empty(StatusCode::NOT_FOUND);
    }
    let methods = ["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT"]
        .into_iter()
        .enumerate()
        .filter_map(|(index, method)| (methods & (1 << index) != 0).then_some(method))
        .collect::<Vec<_>>()
        .join(", ");
    crate::HttpResponse::new(Response::empty(StatusCode::METHOD_NOT_ALLOWED))
        .header("allow", &methods)
        .expect("standard HTTP method names")
        .into_http_response(arena)
}
