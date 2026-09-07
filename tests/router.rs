#![cfg(feature = "macros")]

use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use http_server::__private::{PreparedRoute, RouteDescriptor, RouteMatch};
use http_server::{
    AuthFailure, AuthRequest, Authenticated, Authenticator, Handler, NoAuthenticator, Request,
    Response, Router, Server, ServerConfig, Text, api,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

struct StaticApi;
#[api]
impl StaticApi {
    #[http_server::get("/users/byname")]
    async fn byname(&self) -> Text {
        Text("static".into())
    }
}

struct ParameterApi;
#[api]
impl ParameterApi {
    #[http_server::get("/users/:id")]
    async fn read(&self, id: &str) -> Text {
        tokio::task::yield_now().await;
        Text(format!("read:{id}"))
    }
    #[http_server::post("/users/:id")]
    async fn write(&self, id: &str) -> Text {
        Text(format!("write:{id}"))
    }
}

struct WildcardApi;
#[api]
impl WildcardApi {
    #[http_server::put("/users/*rest")]
    async fn rest(&self, rest: &str) -> Text {
        Text(format!("rest:{rest}"))
    }
}

struct Shard<const N: usize>;
#[api]
impl<const N: usize> Shard<N> {
    #[http_server::get("/shard")]
    async fn read(&self) -> usize {
        N
    }
}

/// Indexed dispatch must never re-enter the old matching methods.
struct IndexedOnly<H>(H, Arc<AtomicUsize>);
impl<A: Authenticator, H: Handler<A>> Handler<A> for IndexedOnly<H> {
    fn routes(&self) -> &'static [RouteDescriptor] {
        self.0.routes()
    }
    fn register_metrics(&self) {
        self.0.register_metrics();
    }
    fn route_priority(&self, _: &str, _: &str) -> Option<usize> {
        panic!("repeated route search")
    }
    fn route_metrics(&self, _: &str, _: &str) -> Option<(usize, http_server::ApiMetrics)> {
        panic!("repeated metric search")
    }
    fn route_methods(&self, _: &str) -> u16 {
        panic!("repeated method search")
    }
    async fn call(&self, _: Request<'_>, _: &A) -> Response {
        panic!("unselected dispatch")
    }
    fn call_route<'a>(
        &'a self,
        request: Request<'a>,
        auth: &'a A,
        route: usize,
        captures: RouteMatch<'a>,
    ) -> impl Future<Output = Response> + Send + 'a {
        self.1.fetch_add(1, Ordering::Relaxed);
        self.0.call_route(request, auth, route, captures)
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
    let router = Router::new(IndexedOnly(StaticApi, invoked.clone())).merge(
        Router::new(IndexedOnly(ParameterApi, invoked.clone()))
            .merge(IndexedOnly(WildcardApi, invoked.clone())),
    );
    // Resolve once before merging to verify warmed indexes are invalidated.
    assert!(router.route_priority("/shard", "GET").is_none());
    let router = router.merge(Shard::<7>);
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
    assert_eq!(prepared.load(Ordering::Relaxed), 11);
    assert_eq!(invoked.load(Ordering::Relaxed), 7);
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
    async fn authenticate(&self, _: AuthRequest<'_>) -> Result<Principal, AuthFailure> {
        Ok(Principal)
    }
}
struct PrivateApi {
    value: &'static str,
}
#[api(auth = required)]
impl PrivateApi {
    #[http_server::get("/private")]
    async fn private(&self, actor: Authenticated<Principal>) -> Text {
        let _ = actor.principal();
        Text(self.value.into())
    }
}

#[tokio::test]
async fn preserves_authenticator_type_and_api_owned_state() {
    let router = Router::new(StaticApi).merge(PrivateApi {
        value: "private-state",
    });
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
    let mut router = Router::new(Shard::<0>);
    for _ in 0..512 {
        router = router.merge(Shard::<1>);
    }
    let nested = Router::new(Shard::<2>).merge(Router::new(Shard::<3>));
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
