#![cfg(feature = "macros")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use http_server::{
    ApiError, ApiResult, AuthFailure, AuthRequest, Authenticated, Authenticator,
    EphemeralBytesArena, Handler, Server, ServerConfig, StatusCode, api,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

#[derive(Deserialize)]
struct UpdateUser<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct UserView<'a> {
    id: u64,
    name: &'a str,
    verbose: bool,
    trace_id: Option<&'a str>,
}

struct UserApi {
    calls: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct Actor {
    id: u64,
}

struct HeaderAuthenticator;

impl Authenticator for HeaderAuthenticator {
    type Principal = Actor;

    async fn authenticate<'a>(&'a self, request: AuthRequest<'a>) -> Result<Actor, AuthFailure> {
        match request.header("authorization") {
            None => Err(AuthFailure::missing_credentials("Bearer")),
            Some(b"Bearer test-token") => Ok(Actor { id: 9 }),
            Some(_) => Err(AuthFailure::invalid_credentials("Bearer")),
        }
    }
}

#[derive(Serialize)]
struct PrivateView {
    id: u64,
    actor_id: u64,
}

#[derive(Serialize)]
struct OptionalAuthView {
    authenticated: bool,
}

#[derive(Serialize)]
struct HealthView {
    ok: bool,
}

struct ProtectedApi;

#[api(prefix = "/private", auth = required)]
impl ProtectedApi {
    #[http_server::get("/:id")]
    async fn get(&self, id: u64, auth: Authenticated<Actor>) -> PrivateView {
        std::future::ready(()).await;
        PrivateView {
            id,
            actor_id: auth.principal().id,
        }
    }

    #[http_server::get("/optional", auth = optional)]
    async fn optional(&self, auth: Option<Authenticated<Actor>>) -> OptionalAuthView {
        std::future::ready(()).await;
        OptionalAuthView {
            authenticated: auth.is_some(),
        }
    }

    #[http_server::get("/health", auth = none)]
    async fn health(&self) -> HealthView {
        std::future::ready(()).await;
        HealthView { ok: true }
    }
}

#[api(prefix = "/v1/users")]
impl UserApi {
    #[http_server::get("/:id")]
    async fn get(&self, id: u64, verbose: Option<bool>) -> UserView<'static> {
        std::future::ready(()).await;
        self.calls.fetch_add(1, Ordering::Relaxed);
        UserView {
            id,
            name: "read",
            verbose: verbose.unwrap_or(false),
            trace_id: None,
        }
    }

    #[http_server::post("/:id", headers(trace_id = "x-trace-id"))]
    async fn update<'a>(
        &self,
        id: u64,
        verbose: Option<bool>,
        input: UpdateUser<'a>,
        trace_id: Option<&'a str>,
    ) -> ApiResult<UserView<'a>> {
        std::future::ready(()).await;
        self.calls.fetch_add(1, Ordering::Relaxed);
        if input.name.is_empty() {
            return Err(ApiError::bad_request("name is required"));
        }
        if input.name == "forbidden" {
            return Err(ApiError::forbidden("not permitted"));
        }
        Ok(UserView {
            id,
            name: input.name,
            verbose: verbose.unwrap_or(false),
            trace_id,
        })
    }
}

fn start_server<H, A>(server: Server<H, A>) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>)
where
    H: Handler<A>,
    A: Authenticator,
{
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .serve_until(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (shutdown_tx, task)
}

async fn request(address: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut client = TcpStream::connect(address).await.unwrap();
    client.write_all(request).await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn macro_binds_path_query_json_body_and_declared_headers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::bind_with_config(
        "127.0.0.1:0".parse().unwrap(),
        UserApi {
            calls: Arc::clone(&calls),
        },
        ServerConfig::default(),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown, task) = start_server(server);

    let response = request(
        address,
        b"POST /v1/users/42?verbose=true HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json; charset=utf-8\r\nX-Trace-Id: trace-7\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"name\":\"alice\"}",
    )
    .await;
    assert_eq!(
        response,
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 60\r\nConnection: close\r\n\r\n{\"id\":42,\"name\":\"alice\",\"verbose\":true,\"trace_id\":\"trace-7\"}"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn macro_rejects_non_json_body_before_invoking_business_code() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::bind_with_config(
        "127.0.0.1:0".parse().unwrap(),
        UserApi {
            calls: Arc::clone(&calls),
        },
        ServerConfig::new(EphemeralBytesArena::new(1024)),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown, task) = start_server(server);

    let response = request(
        address,
        b"POST /v1/users/42 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"name\":\"alice\"}",
    )
    .await;
    assert_eq!(
        response,
        b"HTTP/1.1 415 Unsupported Media Type\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn macro_returns_405_with_methods_for_matched_path() {
    let server = Server::bind_with_config(
        "127.0.0.1:0".parse().unwrap(),
        UserApi {
            calls: Arc::new(AtomicUsize::new(0)),
        },
        ServerConfig::new(EphemeralBytesArena::new(1024)),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown, task) = start_server(server);

    let response = request(
        address,
        b"DELETE /v1/users/42 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        response,
        b"HTTP/1.1 405 Method Not Allowed\r\nallow: GET, POST\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );

    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn macro_serializes_a_business_forbidden_error() {
    let server = Server::bind_with_config(
        "127.0.0.1:0".parse().unwrap(),
        UserApi {
            calls: Arc::new(AtomicUsize::new(0)),
        },
        ServerConfig::new(EphemeralBytesArena::new(1024)),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown, task) = start_server(server);

    let response = request(
        address,
        b"POST /v1/users/42 HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"name\":\"forbidden\"}",
    )
    .await;
    assert_eq!(
        response,
        b"HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{\"error\":\"not permitted\"}"
    );

    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn custom_authenticator_injects_or_rejects_a_typed_principal() {
    let server = Server::bind_with_authenticator_and_config(
        "127.0.0.1:0".parse().unwrap(),
        ProtectedApi,
        HeaderAuthenticator,
        ServerConfig::new(EphemeralBytesArena::new(1024)),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown, task) = start_server(server);

    let missing = request(
        address,
        b"GET /private/7 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        missing,
        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );

    let authenticated = request(
        address,
        b"GET /private/7 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-token\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        authenticated,
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 21\r\nConnection: close\r\n\r\n{\"id\":7,\"actor_id\":9}"
    );

    let optional = request(
        address,
        b"GET /private/optional HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        optional,
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 23\r\nConnection: close\r\n\r\n{\"authenticated\":false}"
    );

    let invalid_optional = request(
        address,
        b"GET /private/optional HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer invalid\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        invalid_optional,
        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );

    let public = request(
        address,
        b"GET /private/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        public,
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}"
    );

    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[test]
fn api_error_preserves_business_status() {
    assert_eq!(
        ApiError::new(StatusCode::CONFLICT, "conflict").to_string(),
        "conflict"
    );
}
