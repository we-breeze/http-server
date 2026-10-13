#![cfg(all(feature = "macros", feature = "metrics"))]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use brz_http_server::{Handler, Response, Server, ServerConfig, StatusCode};
use brz_metrics::MetricSnapshot;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

#[brz_http_server::get("/metric-test/:code", access = public)]
async fn status(code: u16) -> Response {
    Response::empty(StatusCode::from_u16(code).unwrap())
}

#[brz_http_server::post("/metric-test/:code", access = public)]
async fn create(code: u16) -> Response {
    Response::empty(StatusCode::from_u16(code).unwrap())
}

#[brz_http_server::get("/metric-test/secure")]
async fn secure(#[auth] _principal: ()) -> StatusCode {
    StatusCode::OK
}

#[brz_http_server::get("/metric-test/slow", access = public)]
async fn slow() -> StatusCode {
    std::future::pending().await
}

#[brz_http_server::get("/metric-test/invalid-json", access = public)]
async fn invalid_json() -> BTreeMap<Vec<u8>, u8> {
    [(vec![1, 2], 3)].into_iter().collect()
}

#[brz_http_server::get("/metric-test/special", access = public)]
async fn special() -> StatusCode {
    StatusCode::SEE_OTHER
}

brz_http_server::registry!();

fn snapshots() -> BTreeMap<String, MetricSnapshot> {
    let mut metrics = BTreeMap::new();
    brz_metrics::visit(|name, kind, snapshot| {
        if name.starts_with("/metric-test/") {
            if kind != "API" {
                return;
            }
            metrics.insert(format!("{kind} {name}"), snapshot);
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
async fn exported_routes_count_all_statuses_and_timeouts_once() {
    let config = ServerConfig {
        request_timeout: Duration::from_millis(250),
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
    // Binding and explicit registration do not initialize metric slots.
    assert!(snapshots().is_empty());
    brz_http_server::handlers!().unwrap().register_metrics();
    assert!(snapshots().is_empty());

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
    assert_eq!(metrics.len(), paths.len());
    let value = metrics["API /metric-test/:code"];
    assert_eq!(value.total, 14);
    assert_eq!(value.success, 7);
    assert_eq!(value.failure, 7);
    for route in ["secure", "slow", "invalid-json"] {
        let value = metrics[&format!("API /metric-test/{route}")];
        assert_eq!(value.total, 1, "{route}");
        assert_eq!(value.failure, 1, "{route}");
    }
    let timeout = metrics["API /metric-test/slow"];
    assert_eq!(timeout.slow, 1);
    assert_eq!(timeout.elapsed_us, timeout.failure_elapsed_us);
    assert_eq!(timeout.elapsed_us, timeout.slow_elapsed_us);
    let value = metrics["API /metric-test/special"];
    assert_eq!(value.total, 2);
    assert_eq!(value.success, 1);
    assert_eq!(value.failure, 1);
    let mut output = String::new();
    std::io::Read::read_to_string(&mut brz_metrics::PrometheusReader::new(), &mut output).unwrap();
    assert!(!output.contains("status_class="));
    assert!(!output.contains("type=\"API5XX\""));
    for (code, count) in [
        (200, 1),
        (201, 1),
        (204, 1),
        (299, 1),
        (300, 1),
        (304, 1),
        (399, 1),
        (400, 2),
        (405, 1),
        (499, 1),
        (500, 2),
        (599, 1),
    ] {
        let sample = format!(
            "brz_api_resp_total{{type=\"API\",name=\"/metric-test/:code\",status_code=\"{code}\"}} {count}\n"
        );
        assert!(output.contains(&sample), "missing {sample}");
    }
    assert!(!output.contains("status_code=\"600\""));
    assert!(output.contains(
        "brz_api_resp_total{type=\"API\",name=\"/metric-test/slow\",status_code=\"408\"} 1\n"
    ));
    assert!(output.contains("brz_api_timeout_total{type=\"API\",name=\"/metric-test/slow\"} 1\n"));
    assert!(!output.contains("status_code=\"203\""));
    let again = {
        let mut bytes = String::new();
        std::io::Read::read_to_string(&mut brz_metrics::PrometheusReader::new(), &mut bytes)
            .unwrap();
        bytes
    };
    assert_eq!(again, output);
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}
