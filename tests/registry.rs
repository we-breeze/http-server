#![cfg(feature = "macros")]

use brz_http_server::{
    AuthFailure, AuthRequest, Authenticated, Authenticator, Handler, Server, Text,
};
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

struct AppState {
    label: &'static str,
}

brz_http_server::registry!(dependencies(state: Arc<AppState>));
brz_http_server::registry!(group = empty_apis, dependencies(state: Arc<AppState>));
brz_http_server::registry!(group = alternate_apis, dependencies(label: &'static str));
brz_http_server::registry!(group = authenticated_apis, dependencies(label: &'static str), auth = TokenAuth);
brz_http_server::registry!(group = duplicate_apis);
brz_http_server::registry!(group = split_method_apis);
brz_http_server::registry!(group = crossing_apis);
brz_http_server::registry!(group = precedence_apis);
brz_http_server::registry!(group = manual_apis);

mod parcels {
    use super::{AppState, Text};
    #[brz_http_server::get("/fixture/parcels/:parcel")]
    async fn read(#[inject(state)] state: &AppState, parcel: &str) -> Text {
        tokio::task::yield_now().await;
        Text(format!("{}:parcel:{parcel}", state.label))
    }
}
mod lanterns {
    use super::{AppState, Text};
    #[brz_http_server::get("/fixture/lanterns/:lantern")]
    async fn read(lantern: u64, #[inject(state)] application: &AppState) -> Text {
        Text(format!("{}:lantern:{lantern}", application.label))
    }
}

#[brz_http_server::get("/fixture/manual", group = manual_apis)]
async fn manual() -> Text {
    Text("manual".into())
}

// Disabled functions must not register routes or resolve missing types/dependencies.
#[cfg(any())]
#[brz_http_server::get("/fixture/disabled")]
async fn disabled(#[inject(missing)] missing: &MissingType) -> MissingResponse {
    unreachable!()
}

#[brz_http_server::get("/fixture/disabled-after")]
#[cfg(any())]
async fn disabled_after(#[inject(missing)] missing: &MissingType) -> MissingResponse {
    unreachable!()
}

#[brz_http_server::get("/fixture/disabled-conditional")]
#[cfg_attr(all(), cfg(any()))]
async fn disabled_conditional(#[inject(missing)] missing: &MissingType) -> MissingResponse {
    unreachable!()
}

mod northern {
    use super::Text;
    pub(super) type State = &'static str;
    type Auth = brz_http_server::NoAuthenticator;
    brz_http_server::registry!(dependencies(state: State), auth = Auth);
    #[brz_http_server::get("/fixture/nested", group = crate::northern::http_apis)]
    async fn read(#[inject(state)] state: &str) -> Text {
        Text(format!("north:7:{state}"))
    }
}
mod southern {
    use super::Text;
    pub(super) type State = u64;
    brz_http_server::registry!(dependencies(state: State));
    #[brz_http_server::get("/fixture/nested", group = crate::southern::http_apis)]
    async fn read(#[inject(state)] state: State) -> Text {
        Text(format!("south:{state}"))
    }
}
mod alternates {
    use super::Text;
    #[brz_http_server::get("/fixture/alternate", group = alternate_apis)]
    async fn read(#[inject(label)] label: &str) -> Text {
        Text(label.into())
    }
}

struct TokenAuth;
struct Principal(&'static str);
impl Authenticator for TokenAuth {
    type Principal = Principal;
    #[allow(unknown_lints, clippy::unused_async_trait_impl)]
    async fn authenticate(&self, request: AuthRequest<'_>) -> Result<Principal, AuthFailure> {
        match request.header("authorization") {
            Some(b"Bearer fixture-token") => Ok(Principal("accepted")),
            Some(_) => Err(AuthFailure::invalid_credentials("Bearer")),
            None => Err(AuthFailure::missing_credentials("Bearer")),
        }
    }
}

#[brz_http_server::get("/fixture/vault/:slot", group = authenticated_apis, auth = required)]
async fn vault(slot: u32, #[inject(label)] label: &str, actor: Authenticated<Principal>) -> Text {
    Text(format!("{label}:{}:{slot}", actor.principal().0))
}

macro_rules! stateless_api {
    ($name:ident, $registry:path, $method:ident, $path:literal, $parameter:ident, $body:literal) => {
        #[brz_http_server::$method($path, group = $registry)]
        async fn $name($parameter: &str) -> Text {
            Text(format!(concat!($body, ":{}"), $parameter))
        }
    };
}
stateless_api!(
    duplicate_one,
    crate::duplicate_apis,
    get,
    "/echo/:id",
    id,
    "one"
);
stateless_api!(
    duplicate_two,
    crate::duplicate_apis,
    get,
    "/echo/:other",
    other,
    "two"
);
stateless_api!(
    split_get,
    crate::split_method_apis,
    get,
    "/split/:id",
    id,
    "get"
);
stateless_api!(
    split_post,
    crate::split_method_apis,
    post,
    "/split/:other",
    other,
    "post"
);
stateless_api!(
    cross_one,
    crate::crossing_apis,
    get,
    "/cross/:id/fixed",
    id,
    "one"
);
stateless_api!(
    cross_two,
    crate::crossing_apis,
    get,
    "/cross/fixed/:other",
    other,
    "two"
);
stateless_api!(
    choice_parameter,
    crate::precedence_apis,
    get,
    "/choice/:value",
    value,
    "parameter"
);
#[brz_http_server::get("/choice/fixed", group = precedence_apis)]
async fn choice_static() -> Text {
    Text("static".into())
}

async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    authorization: Option<&str>,
) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let auth =
        authorization.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: fixture\r\n{auth}Connection: close\r\nContent-Length: 0\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8(response).unwrap()
}

fn assert_response(response: &str, status: u16, body: &str) {
    assert!(
        response.starts_with(&format!("HTTP/1.1 {status} ")),
        "{response}"
    );
    assert_eq!(response.split_once("\r\n\r\n").unwrap().1, body);
}

#[tokio::test]
async fn discovers_sibling_modules_and_evaluates_dependency_expression_once() {
    let manual: brz_http_server::Router =
        brz_http_server::handlers!(; group = manual_apis).unwrap();
    assert!(manual.route_priority("/fixture/manual", "GET").is_some());
    let evaluations = AtomicUsize::new(0);
    let state = Arc::new(AppState { label: "first" });
    let router = brz_http_server::handlers!(
        state = {
            evaluations.fetch_add(1, Ordering::Relaxed);
            Arc::clone(&state)
        }
    )
    .unwrap();
    assert_eq!(evaluations.load(Ordering::Relaxed), 1);
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), router)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    for _ in 0..2 {
        assert_response(
            &request(address, "GET", "/fixture/parcels/blue", None).await,
            200,
            "first:parcel:blue",
        );
        assert_response(
            &request(address, "GET", "/fixture/lanterns/42", None).await,
            200,
            "first:lantern:42",
        );
    }
    for path in [
        "/fixture/manual",
        "/fixture/alternate",
        "/fixture/disabled",
        "/fixture/disabled-after",
        "/fixture/disabled-conditional",
        "/fixture/nested",
    ] {
        let response = request(address, "GET", path, None).await;
        assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
    }
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn collections_keep_each_construction_state_and_listener_separate() {
    let first = Arc::new(AppState { label: "first" });
    let second = Arc::new(AppState { label: "second" });
    let servers = [
        Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            brz_http_server::handlers!(state = first).unwrap(),
        )
        .await
        .unwrap(),
        Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            brz_http_server::handlers!(state = second).unwrap(),
        )
        .await
        .unwrap(),
        Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            brz_http_server::handlers!(label = "alternate"; group = alternate_apis).unwrap(),
        )
        .await
        .unwrap(),
    ];
    let mut running = Vec::new();
    for server in servers {
        let address = server.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.serve_until(async {
            let _ = rx.await;
        }));
        running.push((address, tx, task));
    }
    for (index, expected) in [(0, "first:parcel:blue"), (1, "second:parcel:blue")] {
        assert_response(
            &request(running[index].0, "GET", "/fixture/parcels/blue", None).await,
            200,
            expected,
        );
    }
    assert_response(
        &request(running[2].0, "GET", "/fixture/alternate", None).await,
        200,
        "alternate",
    );
    let missing = request(running[2].0, "GET", "/fixture/parcels/blue", None).await;
    assert!(missing.starts_with("HTTP/1.1 404 "), "{missing}");
    for (_, tx, task) in running {
        tx.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn registered_handler_keeps_concrete_authenticator_and_rejects_bad_credentials() {
    let router = brz_http_server::handlers!(label = "vault"; group = authenticated_apis).unwrap();
    let server = Server::bind_with_authenticator("127.0.0.1:0".parse().unwrap(), router, TokenAuth)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    for authorization in [None, Some("Bearer incorrect")] {
        let response = request(address, "GET", "/fixture/vault/7", authorization).await;
        assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
        assert!(
            response
                .to_ascii_lowercase()
                .contains("www-authenticate: bearer\r\n")
        );
    }
    assert_response(
        &request(
            address,
            "GET",
            "/fixture/vault/7",
            Some("Bearer fixture-token"),
        )
        .await,
        200,
        "vault:accepted:7",
    );
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn empty_collection_does_not_construct_unrelated_apis() {
    let state = Arc::new(AppState { label: "empty" });
    let router = brz_http_server::handlers!(state; group = empty_apis).unwrap();
    assert!(
        router
            .route_priority("/fixture/parcels/blue", "GET")
            .is_none()
    );
    assert_eq!(router.route_methods("/fixture/parcels/blue"), 0);
}

#[test]
fn equivalent_parameter_patterns_are_rejected_with_both_routes() {
    let result = brz_http_server::handlers!(; group = crate::duplicate_apis);
    let Err(error) = result else {
        panic!("ambiguous routes must be rejected before serving");
    };
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("/echo/:id"), "{diagnostic}");
    assert!(diagnostic.contains("/echo/:other"), "{diagnostic}");
}

#[test]
fn intersecting_patterns_with_equal_specificity_are_rejected() {
    let result = brz_http_server::handlers!(; group = crate::crossing_apis);
    assert!(
        result.is_err(),
        "both routes accept /cross/fixed/fixed with equal priority"
    );
}

#[tokio::test]
async fn disjoint_methods_share_a_path_and_keep_method_errors() {
    let router = brz_http_server::handlers!(; group = crate::split_method_apis).unwrap();
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), router)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    for (method, expected) in [("GET", "get:42"), ("POST", "post:42")] {
        assert_response(
            &request(address, method, "/split/42", None).await,
            200,
            expected,
        );
    }
    let response = request(address, "DELETE", "/split/42", None).await;
    assert!(response.starts_with("HTTP/1.1 405 "), "{response}");
    assert!(response.contains("allow: GET, POST\r\n"), "{response}");
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn overlapping_routes_with_different_specificity_keep_static_precedence() {
    let router = brz_http_server::handlers!(; group = crate::precedence_apis).unwrap();
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), router)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    for (path, expected) in [
        ("/choice/fixed", "static"),
        ("/choice/other", "parameter:other"),
    ] {
        assert_response(&request(address, "GET", path, None).await, 200, expected);
    }
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn nested_same_name_collections_resolve_dependency_type_aliases() {
    let north: northern::State = "blue";
    let south: southern::State = 42;
    let routers = [
        (
            brz_http_server::handlers!(state = north; group = crate::northern::http_apis).unwrap(),
            "north:7:blue",
        ),
        (
            brz_http_server::handlers!(state = south; group = crate::southern::http_apis).unwrap(),
            "south:42",
        ),
    ];
    let mut running = Vec::new();
    for (router, expected) in routers {
        let server = Server::bind("127.0.0.1:0".parse().unwrap(), router)
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.serve_until(async {
            let _ = rx.await;
        }));
        running.push((address, expected, tx, task));
    }
    assert_ne!(running[0].0, running[1].0);
    for (address, expected, _, _) in &running {
        assert_response(
            &request(*address, "GET", "/fixture/nested", None).await,
            200,
            expected,
        );
        let response = request(*address, "GET", "/fixture/parcels/blue", None).await;
        assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
    }
    for (_, _, tx, task) in running {
        tx.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
