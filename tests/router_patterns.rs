#![cfg(feature = "macros")]

//! Synthetic route shapes covering terminal/middle captures and shared prefixes.
//! Business operations are replaced with a route marker and the extracted value.

use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use http_server::{Handler, Router, Server, api};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    task::JoinHandle,
};

macro_rules! fixture_api {
    ($name:ident, $marker:literal, $prefix:literal, $path:literal, $parameter:ident: $ty:ty) => {
        struct $name(Arc<AtomicUsize>);

        #[api(prefix = $prefix, register = false)]
        impl $name {
            #[http_server::get($path)]
            async fn read(&self, $parameter: $ty) -> (usize, String) {
                tokio::task::yield_now().await;
                self.0.fetch_add(1, Ordering::Relaxed);
                ($marker, $parameter.to_string())
            }
        }
    };
}

fixture_api!(ParcelApi, 0, "/svc", "/parcels/:parcel_id", parcel_id: i64);
fixture_api!(BatchEntriesApi, 1, "/svc", "/parcels/batch/:batch_id/entries", batch_id: i64);
fixture_api!(SessionJournalApi, 2, "/svc", "/internal/journal/session/:session_key", session_key: &str);
fixture_api!(PageTextApi, 3, "/svc", "/library/pages/:page_key/text", page_key: &str);
fixture_api!(JobApi, 4, "/svc", "/jobs/:job_id", job_id: i64);
fixture_api!(NodeHealthApi, 5, "/svc", "/jobs/:job_id/remote-node/health", job_id: i64);
fixture_api!(NodeFilesApi, 6, "/svc", "/jobs/:job_id/remote-node/files", job_id: u64);
fixture_api!(EnvironmentApi, 7, "/svc", "/jobs/:job_id/environment-check", job_id: i64);
fixture_api!(JobToolsApi, 8, "/svc", "/jobs/:job_id/tools", job_id: i64);
fixture_api!(ToolPackageApi, 9, "/svc/v2", "/catalog/tools/:tool_id/package", tool_id: i32);
fixture_api!(EventApi, 10, "/svc/v2", "/events/:event_key", event_key: &str);

#[derive(Clone, Copy)]
enum ValueKind {
    Signed,
    Unsigned,
    SmallSigned,
    Text,
}

// Request patterns are independent from generated route descriptors. `$` is
// replaced with a wire value, and the response must identify the intended API.
const CASES: [(&str, ValueKind); 11] = [
    ("/svc/parcels/$", ValueKind::Signed),
    ("/svc/parcels/batch/$/entries", ValueKind::Signed),
    ("/svc/internal/journal/session/$", ValueKind::Text),
    ("/svc/library/pages/$/text", ValueKind::Text),
    ("/svc/jobs/$", ValueKind::Signed),
    ("/svc/jobs/$/remote-node/health", ValueKind::Signed),
    ("/svc/jobs/$/remote-node/files", ValueKind::Unsigned),
    ("/svc/jobs/$/environment-check", ValueKind::Signed),
    ("/svc/jobs/$/tools", ValueKind::Signed),
    ("/svc/v2/catalog/tools/$/package", ValueKind::SmallSigned),
    ("/svc/v2/events/$", ValueKind::Text),
];

fn fixture_router(reverse: bool, calls: &Arc<AtomicUsize>) -> Router {
    let mut groups = vec![
        Router::new(ParcelApi(calls.clone())),
        Router::new(BatchEntriesApi(calls.clone())),
        Router::new(SessionJournalApi(calls.clone())),
        Router::new(PageTextApi(calls.clone())),
        Router::new(JobApi(calls.clone())),
        Router::new(NodeHealthApi(calls.clone())),
        Router::new(NodeFilesApi(calls.clone())),
        Router::new(EnvironmentApi(calls.clone())),
        Router::new(JobToolsApi(calls.clone())),
        Router::new(ToolPackageApi(calls.clone())),
        Router::new(EventApi(calls.clone())),
    ];
    if reverse {
        groups.reverse();
    }
    groups.into_iter().fold(Router::default(), Router::merge)
}

struct Running {
    address: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<http_server::Result<()>>,
}

impl Running {
    async fn start(handler: impl Handler) -> Self {
        let server = Server::bind("127.0.0.1:0".parse().unwrap(), handler)
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(server.serve_until(async {
            let _ = receiver.await;
        }));
        Self {
            address,
            shutdown,
            task,
        }
    }

    async fn request(&self, method: &str, path: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(self.address).await.unwrap();
        stream.write_all(format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, headers.to_owned(), body.to_owned())
    }

    async fn expect_capture(&self, path: &str, marker: usize, value: &str) {
        let (status, _, body) = self.request("GET", path).await;
        assert_eq!(status, 200, "GET {path}: {body}");
        let actual: (usize, String) = serde_json::from_str(&body).unwrap();
        assert_eq!(actual, (marker, value.to_owned()), "GET {path}");
    }

    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn all_eleven_shapes_capture_values_in_both_registration_orders() {
    for reverse in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let server = Running::start(fixture_router(reverse, &calls)).await;
        let mut expected_calls = 0;
        for (marker, (pattern, kind)) in CASES.iter().enumerate() {
            // Vary the total byte length without changing the segment count.
            for value in ["0", "7", "123456789"] {
                server
                    .expect_capture(&pattern.replace('$', value), marker, value)
                    .await;
                expected_calls += 1;
            }
            // Decode before bucketing and keep query slashes out of the path.
            server
                .expect_capture(
                    &format!("{}?ignored=/wrong/branch", pattern.replace('$', "%34%32")),
                    marker,
                    "42",
                )
                .await;
            expected_calls += 1;
            let (wire, expected) = match kind {
                ValueKind::Signed => ("-9223372036854775808", "-9223372036854775808"),
                ValueKind::Unsigned => ("18446744073709551615", "18446744073709551615"),
                ValueKind::SmallSigned => ("-2147483648", "-2147483648"),
                ValueKind::Text => ("opaque-key%2B%E4%B8%AD", "opaque-key+中"),
            };
            server
                .expect_capture(&pattern.replace('$', wire), marker, expected)
                .await;
            expected_calls += 1;
            if matches!(kind, ValueKind::Text) {
                let value = "x".repeat(180);
                server
                    .expect_capture(&pattern.replace('$', &value), marker, &value)
                    .await;
                expected_calls += 1;
            }
        }
        assert_eq!(calls.load(Ordering::Relaxed), expected_calls);
        server.stop().await;
    }
}

#[tokio::test]
async fn malformed_shapes_and_wrong_methods_never_invoke_business_handlers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Running::start(fixture_router(false, &calls)).await;
    for (pattern, kind) in CASES {
        let path = pattern.replace('$', "42");
        for method in ["POST", "DELETE", "PATCH", "HEAD", "OPTIONS", "CUSTOM"] {
            let (status, headers, _) = server.request(method, &path).await;
            assert_eq!(status, 405, "{method} {path}");
            assert!(
                headers.to_ascii_lowercase().contains("\r\nallow: get"),
                "{method} {path}: {headers}"
            );
        }
        for invalid in [
            pattern.replace('$', ""),
            pattern.replace('$', "42/extra"),
            pattern.replace('$', "42%2Fextra"),
            format!("{path}/"),
            format!("{path}/extra"),
            path.replacen("/svc/", "/elsewhere/", 1),
        ] {
            let (status, _, _) = server.request("GET", &invalid).await;
            assert_eq!(status, 404, "GET {invalid}");
        }
        let invalid_value = match kind {
            ValueKind::Signed => Some("9223372036854775808"),
            ValueKind::Unsigned => Some("-1"),
            ValueKind::SmallSigned => Some("2147483648"),
            ValueKind::Text => None,
        };
        if let Some(overflow) = invalid_value {
            for value in ["not-a-number", overflow] {
                let path = pattern.replace('$', value);
                assert_eq!(server.request("GET", &path).await.0, 400, "GET {path}");
            }
        }
    }
    for invalid in [
        "/svc/parcels/batch/42",
        "/svc/jobs/42/remote-node",
        "/svc/jobs/remote-node/42/files",
        "/svc/jobs/42/remote-node/tools",
        "/svc/jobs/42/health",
        "/svc/library/pages/42/entries",
        "/svc/v2/catalog/tools/42/text",
    ] {
        assert_eq!(server.request("GET", invalid).await.0, 404, "GET {invalid}");
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    server.stop().await;
}

struct StaticNeighbors;
#[api(prefix = "/svc", register = false)]
impl StaticNeighbors {
    #[http_server::get("/jobs/search")]
    async fn search(&self) -> &'static str {
        "static-search"
    }

    #[http_server::get("/jobs/42/remote-node/health")]
    async fn health(&self) -> &'static str {
        "static-health"
    }

    #[http_server::post("/jobs/42/tools")]
    async fn tools(&self) -> &'static str {
        "static-tools-post"
    }
}

#[tokio::test]
async fn shared_prefixes_and_suffixes_do_not_steal_each_others_routes() {
    for static_first in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let dynamic = fixture_router(false, &calls);
        let router = if static_first {
            Router::new(StaticNeighbors).merge(dynamic)
        } else {
            dynamic.merge(StaticNeighbors)
        };
        let server = Running::start(router).await;
        for (path, value) in [
            ("/svc/jobs/search", "static-search"),
            ("/svc/jobs/42/remote-node/health", "static-health"),
        ] {
            let (status, _, body) = server.request("GET", path).await;
            assert_eq!(status, 200);
            assert_eq!(serde_json::from_str::<String>(&body).unwrap(), value);
        }
        server.expect_capture("/svc/jobs/42", 4, "42").await;
        server
            .expect_capture("/svc/jobs/43/remote-node/health", 5, "43")
            .await;
        server
            .expect_capture("/svc/jobs/42/remote-node/files", 6, "42")
            .await;
        server
            .expect_capture("/svc/jobs/42/environment-check", 7, "42")
            .await;
        server.expect_capture("/svc/jobs/42/tools", 8, "42").await;
        let (status, _, body) = server.request("POST", "/svc/jobs/42/tools").await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<String>(&body).unwrap(),
            "static-tools-post"
        );
        let (status, headers, _) = server.request("DELETE", "/svc/jobs/42/tools").await;
        assert_eq!(status, 405);
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("\r\nallow: get, post")
        );
        assert_eq!(calls.load(Ordering::Relaxed), 5);
        server.stop().await;
    }
}
