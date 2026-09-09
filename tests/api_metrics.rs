#![cfg(feature = "macros")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use brz_http_server::{Handler, Response, Server, ServerConfig, StatusCode};
use brz_metrics::MetricSnapshot;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

#[brz_http_server::get("/metric-test/:code")]
async fn status(code: u16) -> Response {
    Response::empty(StatusCode::from_u16(code).unwrap())
}

#[brz_http_server::post("/metric-test/:code")]
async fn create(code: u16) -> Response {
    Response::empty(StatusCode::from_u16(code).unwrap())
}

#[brz_http_server::get("/metric-test/secure", auth = required)]
async fn secure() -> StatusCode {
    StatusCode::OK
}

#[brz_http_server::get("/metric-test/slow")]
async fn slow() -> StatusCode {
    std::future::pending().await
}

#[brz_http_server::get("/metric-test/invalid-json")]
async fn invalid_json() -> BTreeMap<Vec<u8>, u8> {
    [(vec![1, 2], 3)].into_iter().collect()
}

#[brz_http_server::get("/metric-test/special")]
async fn special() -> StatusCode {
    StatusCode::SEE_OTHER
}

brz_http_server::registry!();

fn snapshots() -> BTreeMap<String, MetricSnapshot> {
    let mut metrics = BTreeMap::new();
    brz_metrics::visit(|name, kind, snapshot| {
        if name.starts_with("/metric-test/") {
            assert_eq!(kind, "API");
            metrics.insert(name.to_owned(), snapshot);
        }
    });
    metrics
}

async fn send(address: SocketAddr, method: &str, path: &str) -> u16 {
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket.write_all(format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).await.unwrap();
    std::str::from_utf8(&bytes[9..12]).unwrap().parse().unwrap()
}

#[tokio::test]
async fn exported_routes_have_fixed_api_metrics_for_final_status_classes() {
    let config = ServerConfig {
        request_timeout: Duration::from_millis(100),
        ..ServerConfig::default()
    };
    let handler = brz_http_server::handlers!().unwrap();
    let server = Server::bind_with_config("127.0.0.1:0".parse().unwrap(), handler, config)
        .await
        .unwrap();
    let paths = [
        "/metric-test/:code",
        "/metric-test/secure",
        "/metric-test/slow",
        "/metric-test/invalid-json",
        "/metric-test/special",
    ];
    let initial = snapshots();
    assert_eq!(initial.len(), paths.len() * 4);
    for path in paths {
        for class in ["2xx", "3xx", "4xx", "5xx"] {
            assert_eq!(initial[&format!("{path}_{class}")].total, 0);
        }
    }
    // Repeated registration, including a shared path with two methods, deduplicates slots.
    brz_http_server::handlers!().unwrap().register_metrics();
    assert_eq!(snapshots().len(), initial.len());

    let address = server.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    for status in [200, 204, 299, 300, 304, 399, 400, 499, 500, 599] {
        assert_eq!(
            send(
                address,
                "GET",
                &format!("/metric-test/{status}?ignored=yes")
            )
            .await,
            status
        );
    }
    assert_eq!(send(address, "POST", "/metric-test/201").await, 201);
    assert_eq!(send(address, "GET", "/metric-test/not-a-number").await, 400);
    assert_eq!(send(address, "DELETE", "/metric-test/200").await, 405);
    assert_eq!(send(address, "GET", "/metric-test/600").await, 500);
    assert_eq!(send(address, "GET", "/metric-test/secure").await, 401);
    assert_eq!(send(address, "GET", "/metric-test/slow").await, 408);
    assert_eq!(send(address, "GET", "/metric-test/invalid-json").await, 500);
    assert_eq!(send(address, "GET", "/metric-test/special").await, 303);
    assert_eq!(send(address, "DELETE", "/metric-test/special").await, 405);
    assert_eq!(send(address, "GET", "/unmatched-metric-test").await, 404);

    let metrics = snapshots();
    assert_eq!(metrics.len(), initial.len());
    for (class, count) in [("2xx", 4), ("3xx", 3), ("4xx", 4), ("5xx", 3)] {
        let value = metrics[&format!("/metric-test/:code_{class}")];
        assert_eq!(value.total, count, "{class}");
        assert_eq!(
            value.failure,
            if class == "4xx" || class == "5xx" {
                count
            } else {
                0
            }
        );
    }
    assert_eq!(metrics["/metric-test/secure_4xx"].total, 1);
    assert_eq!(metrics["/metric-test/slow_4xx"].total, 1);
    assert_eq!(metrics["/metric-test/invalid-json_5xx"].total, 1);
    assert_eq!(metrics["/metric-test/special_3xx"].total, 1);
    assert_eq!(metrics["/metric-test/special_4xx"].total, 1);
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}
