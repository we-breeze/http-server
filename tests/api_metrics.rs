#![cfg(feature = "macros")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use brz_metrics::MetricSnapshot;
use http_server::{Handler, Response, Router, Server, ServerConfig, StatusCode, api};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

struct StatusApi;

#[api(prefix = "/metric-test")]
impl StatusApi {
    #[http_server::get("/:code")]
    async fn status(&self, code: u16) -> Response {
        Response::empty(StatusCode::from_u16(code).unwrap())
    }

    #[http_server::post("/:code")]
    async fn create(&self, code: u16) -> Response {
        Response::empty(StatusCode::from_u16(code).unwrap())
    }

    #[http_server::get("/secure", auth = required)]
    async fn secure(&self) -> StatusCode {
        StatusCode::OK
    }

    #[http_server::get("/slow")]
    async fn slow(&self) -> StatusCode {
        std::future::pending().await
    }

    #[http_server::get("/invalid-json")]
    async fn invalid_json(&self) -> BTreeMap<Vec<u8>, u8> {
        [(vec![1, 2], 3)].into_iter().collect()
    }
}

struct StaticApi;

#[api(prefix = "/metric-test")]
impl StaticApi {
    #[http_server::get("/special")]
    async fn special(&self) -> StatusCode {
        StatusCode::SEE_OTHER
    }
}

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
    let handler = Router::new(StatusApi).merge(StaticApi);
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
    <StatusApi as Handler>::register_metrics(&StatusApi);
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
