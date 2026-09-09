#![cfg(feature = "macros")]

use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use brz_http_server::__private::{PreparedRoute, RouteDescriptor, RouteMatch};
use brz_http_server::{
    AuthFailure, AuthRequest, Authenticated, Authenticator, Handler, IntoHttpResponse,
    NoAuthenticator, Request, Response, Router, Server, ServerConfig, Text,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

brz_http_server::registry!(dependencies(calls: Arc<AtomicUsize>));
brz_http_server::registry!(group = shards, dependencies(number: usize));
brz_http_server::registry!(group = private, dependencies(value: &'static str), auth = Auth);

#[brz_http_server::get("/users/byname")]
async fn byname(#[inject(calls)] calls: &AtomicUsize) -> Text {
    calls.fetch_add(1, Ordering::Relaxed);
    Text("static".into())
}

#[brz_http_server::get("/users/:id")]
async fn read(#[inject(calls)] calls: &AtomicUsize, id: &str) -> Text {
    calls.fetch_add(1, Ordering::Relaxed);
    tokio::task::yield_now().await;
    Text(format!("read:{id}"))
}

#[brz_http_server::post("/users/:id")]
async fn write(#[inject(calls)] calls: &AtomicUsize, id: &str) -> Text {
    calls.fetch_add(1, Ordering::Relaxed);
    Text(format!("write:{id}"))
}

#[brz_http_server::put("/users/*rest")]
async fn rest(#[inject(calls)] calls: &AtomicUsize, rest: &str) -> Text {
    calls.fetch_add(1, Ordering::Relaxed);
    Text(format!("rest:{rest}"))
}

#[brz_http_server::get("/shard", group = shards)]
async fn shard(#[inject(number)] number: usize) -> usize {
    number
}

// A descriptor-backed handler must be invoked with the prepared captures;
// calling any fallback matching hook would repeat the route search.
struct IndexedOnly(Arc<AtomicUsize>);
impl Handler for IndexedOnly {
    fn routes(&self) -> &'static [RouteDescriptor] {
        const ROUTES: &[RouteDescriptor] = &[RouteDescriptor {
            path: "/indexed/:id",
            methods: 2,
            priority: 1 << 24,
            metrics: || {
                brz_http_server::ApiMetrics::new([
                    "/indexed/:id_2xx",
                    "/indexed/:id_3xx",
                    "/indexed/:id_4xx",
                    "/indexed/:id_5xx",
                ])
            },
        }];
        ROUTES
    }
    fn route_priority(&self, _: &str, _: &str) -> Option<usize> {
        panic!("repeated route search")
    }
    fn route_metrics(&self, _: &str, _: &str) -> Option<(usize, brz_http_server::ApiMetrics)> {
        panic!("repeated metric search")
    }
    fn route_methods(&self, _: &str) -> u16 {
        panic!("repeated method search")
    }
    #[allow(unknown_lints, clippy::unused_async_trait_impl)]
    async fn call(&self, _: Request<'_>, _: &NoAuthenticator) -> Response {
        panic!("unselected dispatch")
    }
    #[allow(unknown_lints, clippy::unused_async_trait_impl)]
    async fn call_route<'a>(
        &'a self,
        request: Request<'a>,
        _: &'a NoAuthenticator,
        route: usize,
        captures: RouteMatch<'a>,
    ) -> Response {
        assert_eq!(route, 0);
        self.0.fetch_add(1, Ordering::Relaxed);
        Text(format!("indexed:{}", captures.capture(0).unwrap()))
            .into_http_response(request.response_arena())
    }
}

struct CountPrepared {
    router: Router,
    calls: Arc<AtomicUsize>,
}
impl Handler for CountPrepared {
    fn register_metrics(&self) {
        self.router.register_metrics();
    }
    fn prepare<'p>(&self, path: &'p str, method: &str) -> PreparedRoute<'p> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.router.prepare(path, method)
    }
    fn call_prepared<'a>(
        &'a self,
        request: Request<'a>,
        auth: &'a NoAuthenticator,
        prepared: &'a PreparedRoute<'_>,
    ) -> impl Future<Output = Response> + Send + 'a {
        self.router.call_prepared(request, auth, prepared)
    }
    // Keep fixtures on the same async trait API as real handlers.
    #[allow(unknown_lints, clippy::unused_async_trait_impl)]
    async fn call(&self, _: Request<'_>, _: &NoAuthenticator) -> Response {
        panic!("server discarded prepared route")
    }
}

async fn send(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    content_length: usize,
) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: {content_length}\r\n\r\n").as_bytes()).await.unwrap();
    let mut result = Vec::new();
    stream.read_to_end(&mut result).await.unwrap();
    String::from_utf8(result).unwrap()
}

#[tokio::test]
async fn resolves_once_and_dispatches_static_parameters_and_wildcards() {
    let invoked = Arc::new(AtomicUsize::new(0));
    let prepared = Arc::new(AtomicUsize::new(0));
    let router = brz_http_server::handlers!(calls = invoked.clone())
        .unwrap()
        .merge(IndexedOnly(invoked.clone()));
    // Resolve once before merging to verify warmed indexes are invalidated.
    assert!(router.route_priority("/shard", "GET").is_none());
    let router = router.merge(brz_http_server::handlers!(number = 7; group = shards).unwrap());
    let server = Server::bind_with_config(
        "127.0.0.1:0".parse().unwrap(),
        CountPrepared {
            router,
            calls: prepared.clone(),
        },
        ServerConfig {
            request_timeout: Duration::from_millis(100),
            ..ServerConfig::default()
        },
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    for (method, path, suffix) in [
        ("GET", "/users/byname", "static"),
        ("POST", "/users/byname", "write:byname"),
        ("GET", "/users/123", "read:123"),
        ("GET", "/users/%E4%B8%AD", "read:中"),
        ("PUT", "/users", "rest:"),
        ("PUT", "/users/", "rest:"),
        ("PUT", "/users/a%2Fb", "rest:a/b"),
        ("GET", "/shard", "7"),
        ("GET", "/indexed/9", "indexed:9"),
    ] {
        let response = send(address, method, path, 0).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with(suffix), "{response}");
    }
    let wrong_method = send(address, "DELETE", "/users/byname", 0).await;
    assert!(wrong_method.starts_with("HTTP/1.1 405"));
    assert!(
        wrong_method.contains("allow: GET, POST, PUT\r\n"),
        "{wrong_method}"
    );
    assert!(
        send(address, "GET", "/unknown", 0)
            .await
            .starts_with("HTTP/1.1 404")
    );
    assert!(
        send(address, "POST", "/users/body-timeout", 10)
            .await
            .starts_with("HTTP/1.1 408")
    );
    assert_eq!(prepared.load(Ordering::Relaxed), 12);
    assert_eq!(invoked.load(Ordering::Relaxed), 8);
    let mut recorded = false;
    brz_metrics::visit(|name, _, snapshot| {
        if name == "/users/:id_4xx" {
            recorded = snapshot.total >= 1;
        }
    });
    assert!(
        recorded,
        "body-read timeout must retain the matched route's metrics"
    );
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

struct Principal;
struct Auth;
impl Authenticator for Auth {
    type Principal = Principal;
    // Keep fixtures on the same async trait API as real handlers.
    #[allow(unknown_lints, clippy::unused_async_trait_impl)]
    async fn authenticate(&self, _: AuthRequest<'_>) -> Result<Principal, AuthFailure> {
        Ok(Principal)
    }
}
#[brz_http_server::get("/private", group = private, auth = required)]
async fn private(#[inject(value)] value: &str, actor: Authenticated<Principal>) -> Text {
    let _ = actor.principal();
    Text(value.into())
}

#[brz_http_server::get("/users/byname", group = private)]
async fn public() -> Text {
    Text("static".into())
}

#[tokio::test]
async fn preserves_authenticator_type_and_named_dependencies() {
    let router = brz_http_server::handlers!(value = "private-state"; group = private).unwrap();
    let server = Server::bind_with_authenticator("127.0.0.1:0".parse().unwrap(), router, Auth)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    assert!(
        send(address, "GET", "/private", 0)
            .await
            .ends_with("private-state")
    );
    assert!(
        send(address, "GET", "/users/byname", 0)
            .await
            .ends_with("static")
    );
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn hundreds_of_apis_keep_one_router_type_and_registration_order() {
    // This reassignment could not compile with Router<Self, H>.
    let mut router = Router::new(brz_http_server::handlers!(number = 0; group = shards).unwrap());
    for _ in 0..512 {
        router = router.merge(brz_http_server::handlers!(number = 1; group = shards).unwrap());
    }
    let nested = Router::new(brz_http_server::handlers!(number = 2; group = shards).unwrap())
        .merge(Router::new(
            brz_http_server::handlers!(number = 3; group = shards).unwrap(),
        ));
    let router = router.merge(nested);
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), router)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    assert!(send(address, "GET", "/shard", 0).await.ends_with('0'));
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}
