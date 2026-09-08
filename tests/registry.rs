#![cfg(feature = "macros")]

use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use brz_http_server::{
    AuthFailure, AuthRequest, Authenticated, Authenticator, FromState, Handler, Server, Text, api,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

struct AppState {
    label: &'static str,
    constructions: Arc<AtomicUsize>,
}

brz_http_server::registry!(state = Arc<AppState>);
brz_http_server::registry!(name = empty_apis, state = Arc<AppState>);
brz_http_server::registry!(group = alternate_apis, state = &'static str);
brz_http_server::registry!(group = authenticated_apis, state = &'static str, auth = TokenAuth);
brz_http_server::registry!(name = duplicate_apis, state = ());
brz_http_server::registry!(name = split_method_apis, state = ());
brz_http_server::registry!(name = crossing_apis, state = ());
brz_http_server::registry!(name = precedence_apis, state = ());

// Registration is local to each API module. The entry point never lists the
// types, and each API can construct a different representation of shared state.
mod parcels {
    use super::{AppState, Arc, FromState, Ordering, Text, api};

    pub(super) struct ParcelApi(Arc<AppState>);

    impl FromState<Arc<AppState>> for ParcelApi {
        fn from_state(state: &Arc<AppState>) -> Self {
            state.constructions.fetch_add(1, Ordering::Relaxed);
            Self(Arc::clone(state))
        }
    }

    #[api]
    impl ParcelApi {
        #[brz_http_server::get("/fixture/parcels/:parcel")]
        async fn read(&self, parcel: &str) -> Text {
            tokio::task::yield_now().await;
            Text(format!("{}:parcel:{parcel}", self.0.label))
        }
    }
}

mod lanterns {
    use super::{AppState, Arc, FromState, Ordering, Text, api};

    struct LanternState {
        label: String,
    }

    pub(super) struct LanternApi(LanternState);

    impl FromState<Arc<AppState>> for LanternApi {
        fn from_state(state: &Arc<AppState>) -> Self {
            state.constructions.fetch_add(1, Ordering::Relaxed);
            Self(LanternState {
                label: state.label.to_owned(),
            })
        }
    }

    #[api(prefix = "/fixture")]
    impl LanternApi {
        #[brz_http_server::get("/lanterns/:lantern")]
        async fn read(&self, lantern: u64) -> Text {
            Text(format!("{}:lantern:{lantern}", self.0.label))
        }
    }
}

struct ManualOnly;

#[api(register = false)]
impl ManualOnly {
    #[brz_http_server::get("/fixture/manual")]
    async fn read(&self) -> Text {
        Text("manual".into())
    }
}

// Disabled declarations must not register constructors or require a state
// conversion (or even resolve the absent type).
#[cfg(any())]
#[api]
impl UnavailableApi {
    #[brz_http_server::get("/fixture/disabled")]
    async fn read(&self) -> MissingResponse {
        unreachable!()
    }
}

#[api]
#[cfg(any())]
impl UnavailableAfterApi {
    #[brz_http_server::get("/fixture/disabled-after")]
    async fn read(&self) -> MissingResponse {
        unreachable!()
    }
}

#[api]
#[cfg_attr(all(), cfg(any()))]
impl UnavailableConditionalApi {
    #[brz_http_server::get("/fixture/disabled-conditional")]
    async fn read(&self) -> MissingResponse {
        unreachable!()
    }
}

mod northern {
    use super::{FromState, Text, api};

    pub(super) type State = &'static str;
    type Auth = brz_http_server::NoAuthenticator;

    brz_http_server::registry!(state = State, auth = Auth);

    #[derive(FromState)]
    struct Api<const N: usize> {
        state: State,
    }

    // A concrete specialization can register without a generic factory.
    #[api(group = crate::northern::http_apis)]
    impl Api<7> {
        #[brz_http_server::get("/fixture/nested")]
        async fn read(&self) -> Text {
            Text(format!("north:7:{}", self.state))
        }
    }
}

mod southern {
    use super::{FromState, Text, api};

    pub(super) type State = u64;

    // This identically named collection has an incompatible state type and
    // the same route, making accidental cross-registration observable.
    brz_http_server::registry!(state = State);

    struct Api(State);

    impl FromState<State> for Api {
        fn from_state(state: &State) -> Self {
            Self(*state)
        }
    }

    #[api(group = crate::southern::http_apis)]
    impl Api {
        #[brz_http_server::get("/fixture/nested")]
        async fn read(&self) -> Text {
            Text(format!("south:{}", self.0))
        }
    }
}

mod alternates {
    use super::{FromState, Text, api};

    struct AlternateApi(&'static str);

    impl FromState<&'static str> for AlternateApi {
        fn from_state(state: &&'static str) -> Self {
            Self(state)
        }
    }

    #[api(prefix = "/fixture", group = alternate_apis)]
    impl AlternateApi {
        #[brz_http_server::get("/alternate")]
        async fn read(&self) -> Text {
            Text(self.0.into())
        }
    }
}

struct TokenAuth;
struct Principal(&'static str);

impl Authenticator for TokenAuth {
    type Principal = Principal;

    async fn authenticate(&self, request: AuthRequest<'_>) -> Result<Principal, AuthFailure> {
        match request.header("authorization") {
            Some(b"Bearer fixture-token") => Ok(Principal("accepted")),
            Some(_) => Err(AuthFailure::invalid_credentials("Bearer")),
            None => Err(AuthFailure::missing_credentials("Bearer")),
        }
    }
}

struct ProtectedApi(&'static str);

impl FromState<&'static str> for ProtectedApi {
    fn from_state(state: &&'static str) -> Self {
        Self(state)
    }
}

#[api(
    prefix = "/fixture",
    group = authenticated_apis,
    auth = required
)]
impl ProtectedApi {
    #[brz_http_server::get("/vault/:slot")]
    async fn read(&self, slot: u32, actor: Authenticated<Principal>) -> Text {
        Text(format!("{}:{}:{slot}", self.0, actor.principal().0))
    }
}

macro_rules! stateless_api {
    ($name:ident, $registry:path, $method:ident, $path:literal, $parameter:ident, $body:literal) => {
        struct $name;

        impl FromState<()> for $name {
            fn from_state((): &()) -> Self {
                Self
            }
        }

        #[api(registry = $registry)]
        impl $name {
            #[brz_http_server::$method($path)]
            async fn read(&self, $parameter: &str) -> Text {
                Text(format!(concat!($body, ":{}"), $parameter))
            }
        }
    };
}

stateless_api!(
    DuplicateOne,
    crate::duplicate_apis,
    get,
    "/echo/:id",
    id,
    "one"
);
stateless_api!(
    DuplicateTwo,
    crate::duplicate_apis,
    get,
    "/echo/:other",
    other,
    "two"
);
stateless_api!(
    SplitGet,
    crate::split_method_apis,
    get,
    "/split/:id",
    id,
    "get"
);
stateless_api!(
    SplitPost,
    crate::split_method_apis,
    post,
    "/split/:other",
    other,
    "post"
);
stateless_api!(
    CrossOne,
    crate::crossing_apis,
    get,
    "/cross/:id/fixed",
    id,
    "one"
);
stateless_api!(
    CrossTwo,
    crate::crossing_apis,
    get,
    "/cross/fixed/:other",
    other,
    "two"
);
stateless_api!(
    ChoiceParameter,
    crate::precedence_apis,
    get,
    "/choice/:value",
    value,
    "parameter"
);

struct ChoiceStatic;

impl FromState<()> for ChoiceStatic {
    fn from_state((): &()) -> Self {
        Self
    }
}

#[api(registry = crate::precedence_apis)]
impl ChoiceStatic {
    #[brz_http_server::get("/choice/fixed")]
    async fn read(&self) -> Text {
        Text("static".into())
    }
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
async fn discovers_sibling_modules_and_constructs_once_from_one_state_expression() {
    let manual: brz_http_server::Router = brz_http_server::Router::new(ManualOnly);
    assert!(manual.route_priority("/fixture/manual", "GET").is_some());
    let constructions = Arc::new(AtomicUsize::new(0));
    let evaluations = AtomicUsize::new(0);
    let state = Arc::new(AppState {
        label: "first",
        constructions: Arc::clone(&constructions),
    });
    let router = brz_http_server::handlers!({
        evaluations.fetch_add(1, Ordering::Relaxed);
        Arc::clone(&state)
    })
    .unwrap();
    assert_eq!(evaluations.load(Ordering::Relaxed), 1);
    assert_eq!(constructions.load(Ordering::Relaxed), 2);
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
    assert_eq!(constructions.load(Ordering::Relaxed), 2);
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn collections_keep_each_construction_state_and_listener_separate() {
    let constructions = Arc::new(AtomicUsize::new(0));
    let first = Arc::new(AppState {
        label: "first",
        constructions: Arc::clone(&constructions),
    });
    let second = Arc::new(AppState {
        label: "second",
        constructions: Arc::clone(&constructions),
    });
    let servers = [
        Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            brz_http_server::handlers!(first).unwrap(),
        )
        .await
        .unwrap(),
        Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            brz_http_server::handlers!(second).unwrap(),
        )
        .await
        .unwrap(),
        Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            brz_http_server::handlers!("alternate", alternate_apis).unwrap(),
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
    assert_eq!(constructions.load(Ordering::Relaxed), 4);
    for (_, tx, task) in running {
        tx.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn registered_handler_keeps_concrete_authenticator_and_rejects_bad_credentials() {
    let router = brz_http_server::handlers!("vault", authenticated_apis).unwrap();
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
    let constructions = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(AppState {
        label: "empty",
        constructions: Arc::clone(&constructions),
    });
    let router = brz_http_server::handlers!(state, registry = crate::empty_apis).unwrap();
    assert!(
        router
            .route_priority("/fixture/parcels/blue", "GET")
            .is_none()
    );
    assert_eq!(router.route_methods("/fixture/parcels/blue"), 0);
    assert_eq!(constructions.load(Ordering::Relaxed), 0);
}

#[test]
fn equivalent_parameter_patterns_are_rejected_with_both_routes() {
    let result = brz_http_server::handlers!((), registry = crate::duplicate_apis);
    let Err(error) = result else {
        panic!("ambiguous routes must be rejected before serving");
    };
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("/echo/:id"), "{diagnostic}");
    assert!(diagnostic.contains("/echo/:other"), "{diagnostic}");
}

#[test]
fn intersecting_patterns_with_equal_specificity_are_rejected() {
    let result = brz_http_server::handlers!((), registry = crate::crossing_apis);
    assert!(
        result.is_err(),
        "both routes accept /cross/fixed/fixed with equal priority"
    );
}

#[tokio::test]
async fn disjoint_methods_share_a_path_and_keep_method_errors() {
    let router = brz_http_server::handlers!((), registry = crate::split_method_apis).unwrap();
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
    let router = brz_http_server::handlers!((), registry = crate::precedence_apis).unwrap();
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
async fn nested_same_name_collections_resolve_aliases_and_concrete_generic_apis() {
    let north: northern::State = "blue";
    let south: southern::State = 42;
    let routers = [
        (
            brz_http_server::handlers!(north, crate::northern::http_apis).unwrap(),
            "north:7:blue",
        ),
        (
            brz_http_server::handlers!(south, crate::southern::http_apis).unwrap(),
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
