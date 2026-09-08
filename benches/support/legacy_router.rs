// Pre-change Router, retained only as a routing benchmark baseline.
use brz_http_server::{Authenticator, Handler, IntoHttpResponse, Request, Response, StatusCode};

/// Statically composes macro-exported API groups on one listener.
pub struct Router<L, R = EmptyRoutes> {
    left: L,
    right: R,
}

impl<H> Router<H> {
    #[must_use]
    pub fn new(handler: H) -> Self {
        Self {
            left: handler,
            right: EmptyRoutes,
        }
    }
}

impl<L, R> Router<L, R> {
    #[must_use]
    pub fn merge<H>(self, handler: H) -> Router<Self, H> {
        Router {
            left: self,
            right: handler,
        }
    }
}

impl<A: Authenticator, L: Handler<A>, R: Handler<A>> Handler<A> for Router<L, R> {
    fn register_metrics(&self) {
        self.left.register_metrics();
        self.right.register_metrics();
    }

    fn route_metrics(
        &self,
        path: &str,
        method: &str,
    ) -> Option<(usize, brz_http_server::ApiMetrics)> {
        let left = self.left.route_priority(path, method);
        let right = self.right.route_priority(path, method);
        if right > left {
            self.right.route_metrics(path, method)
        } else if left.is_some() {
            self.left.route_metrics(path, method)
        } else {
            let left = self.left.route_metrics(path, method);
            let right = self.right.route_metrics(path, method);
            if right.as_ref().map(|(priority, _)| priority)
                > left.as_ref().map(|(priority, _)| priority)
            {
                right
            } else {
                left
            }
        }
    }

    fn route_priority(&self, path: &str, method: &str) -> Option<usize> {
        self.left
            .route_priority(path, method)
            .max(self.right.route_priority(path, method))
    }

    fn route_methods(&self, path: &str) -> u16 {
        self.left.route_methods(path) | self.right.route_methods(path)
    }

    // Keep fixtures on the same async trait API as real handlers.
    #[allow(unknown_lints, clippy::unused_async_trait_impl)]
    async fn call(&self, request: Request<'_>, authenticator: &A) -> Response {
        let left = self.left.route_priority(request.path(), request.method());
        let right = self.right.route_priority(request.path(), request.method());
        if right > left {
            self.right.call(request, authenticator).await
        } else if left.is_some() {
            self.left.call(request, authenticator).await
        } else {
            unmatched(self.route_methods(request.path()), request.response_arena())
        }
    }
}

#[doc(hidden)]
pub struct EmptyRoutes;

impl<A: Authenticator> Handler<A> for EmptyRoutes {
    // Keep fixtures on the same async trait API as real handlers.
    #[allow(unknown_lints, clippy::unused_async_trait_impl)]
    async fn call(&self, _request: Request<'_>, _authenticator: &A) -> Response {
        Response::empty(StatusCode::NOT_FOUND)
    }
}

/// Generates the shared 404/405 response after checking all matching routes.
#[doc(hidden)]
#[must_use]
pub fn unmatched(methods: u16, arena: &brz_http_server::EphemeralBytesArena) -> Response {
    if methods == 0 {
        return Response::empty(StatusCode::NOT_FOUND);
    }
    let methods = ["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT"]
        .into_iter()
        .enumerate()
        .filter_map(|(index, method)| (methods & (1 << index) != 0).then_some(method))
        .collect::<Vec<_>>()
        .join(", ");
    brz_http_server::HttpResponse::new(Response::empty(StatusCode::METHOD_NOT_ALLOWED))
        .header("allow", &methods)
        .expect("standard HTTP method names")
        .into_http_response(arena)
}
