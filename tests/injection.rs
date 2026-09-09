#![cfg(feature = "macros")]

use brz_http_server::{ApiResult, Server, Text};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

// Neither AppState nor Store implements Clone. The registry owns one container.
struct AppState {
    name: String,
}
struct Store {
    name: &'static str,
}
type StoreHandle = Arc<Store>;

struct CloneProbe(Arc<AtomicUsize>);
impl Clone for CloneProbe {
    fn clone(&self) -> Self {
        self.0.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(&self.0))
    }
}
impl CloneProbe {
    #[allow(dead_code, clippy::unused_self)]
    fn clone(&self) -> Self {
        panic!("injection must use the Clone trait")
    }
}

brz_http_server::registry!(dependencies(
    state: AppState,
    primary: StoreHandle,
    replica: StoreHandle,
    probe: CloneProbe,
    r#type: u32,
));

#[derive(Deserialize)]
struct Input<'a> {
    name: &'a str,
}
#[derive(Serialize)]
struct View<'a> {
    application: &'a str,
    input: &'a str,
    trace: &'a str,
    id: u64,
}

#[brz_http_server::post("/borrow/:id", headers(trace = "x-trace"))]
async fn borrowed<'a>(
    #[inject(state)] application: &'a AppState,
    id: u64,
    #[inject(replica)] _reader: &Store,
    input: Input<'a>,
    trace: &'a str,
) -> ApiResult<View<'a>> {
    tokio::task::yield_now().await;
    Ok(View {
        application: &application.name,
        input: input.name,
        trace,
        id,
    })
}

#[brz_http_server::get("/stores")]
async fn stores(
    #[inject(replica)] reader: &Store,
    #[inject(primary)] writer: &Store,
    #[inject(probe)] probe: &CloneProbe,
) -> Text {
    assert_eq!(probe.0.load(Ordering::Relaxed), 0);
    Text(format!("{}:{}", reader.name, writer.name))
}

#[brz_http_server::get("/owned")]
async fn owned(#[inject(probe)] probe: CloneProbe, #[inject(primary)] store: StoreHandle) -> Text {
    tokio::task::yield_now().await;
    Text(format!(
        "{}:{}",
        store.name,
        probe.0.load(Ordering::Relaxed)
    ))
}

// Function and parameter names may coincide, including generated-looking names.
#[brz_http_server::get("/echo")]
async fn echo(
    #[inject(state)] echo: &AppState,
    __http_business_function: &str,
    __http_request: Option<u32>,
    #[inject(r#type)] value: u32,
) -> Text {
    assert_eq!(__http_request, None);
    Text(format!("{}:{__http_business_function}:{value}", echo.name))
}

#[brz_http_server::get("/health")]
async fn health() -> bool {
    true
}

#[brz_http_server::get("/name")]
#[cfg_attr(all(), inline)]
async fn name(#[inject(state)] state: &AppState) -> &str {
    &state.name
}

#[brz_http_server::get("/mutable")]
async fn mutable(mut value: String) -> String {
    value.push('!');
    value
}

async fn request(address: std::net::SocketAddr, wire: &str) -> String {
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket.write_all(wire.as_bytes()).await.unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    response.split_once("\r\n\r\n").unwrap().1.to_owned()
}

#[tokio::test]
async fn dependencies_are_named_shared_and_borrowed_across_awaits() {
    let clones = Arc::new(AtomicUsize::new(0));
    let primary = Arc::new(Store { name: "primary" });
    let replica = Arc::new(Store { name: "replica" });
    let evaluations = AtomicUsize::new(0);
    let handler = brz_http_server::handlers!(
        replica = Arc::clone(&replica),
        state = {
            evaluations.fetch_add(1, Ordering::Relaxed);
            AppState {
                name: "application".into(),
            }
        },
        primary = Arc::clone(&primary),
        probe = CloneProbe(Arc::clone(&clones)),
        r#type = 7,
    )
    .unwrap();
    assert_eq!(evaluations.load(Ordering::Relaxed), 1);
    assert_eq!(clones.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&primary), 2);
    assert_eq!(Arc::strong_count(&replica), 2);
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), handler)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    for _ in 0..2 {
        assert_eq!(
            request(
                address,
                "GET /stores HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
            )
            .await,
            "replica:primary"
        );
        let response = request(address, "POST /borrow/42 HTTP/1.1\r\nHost: test\r\nX-Trace: trace\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"name\":\"alice\"}").await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
            serde_json::json!({"application":"application","input":"alice","trace":"trace","id":42})
        );
    }
    assert_eq!(request(address, "GET /echo?__http_business_function=query HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").await, "application:query:7");
    assert_eq!(
        request(
            address,
            "GET /name HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
        )
        .await,
        "\"application\""
    );
    assert_eq!(
        request(
            address,
            "GET /mutable?value=hello HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
        )
        .await,
        "\"hello!\""
    );
    assert_eq!(clones.load(Ordering::Relaxed), 0);
    assert_eq!(
        request(
            address,
            "GET /owned HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
        )
        .await,
        "primary:1"
    );
    assert_eq!(clones.load(Ordering::Relaxed), 1);
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(Arc::strong_count(&primary), 1);
    assert_eq!(Arc::strong_count(&replica), 1);
}

#[tokio::test]
async fn exported_functions_remain_directly_callable() {
    let application = AppState {
        name: "direct".into(),
    };
    let replica = Store { name: "replica" };
    let result = borrowed(&application, 8, &replica, Input { name: "input" }, "trace")
        .await
        .unwrap();
    assert_eq!(result.application, "direct");
    assert_eq!(result.input, "input");
    assert_eq!(result.id, 8);
    assert!(health().await);
}
