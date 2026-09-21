#![cfg(feature = "macros")]

//! Synthetic route shapes covering terminal/middle captures and shared prefixes.
//! Business operations are replaced with a route marker and the extracted value.

use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use brz_http_server::{Handler, Router, Server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    task::JoinHandle,
};

brz_http_server::registry!(group = static_neighbors);

macro_rules! fixture_api {
    ($name:ident, $marker:literal, $path:literal, $parameter:ident: $ty:ty) => {
        mod $name {
            use super::*;
            brz_http_server::registry!(dependencies(calls: Arc<AtomicUsize>));
            #[brz_http_server::get(
                $path,
                access = public,
                group = crate::$name::http_apis
            )]
            async fn read(#[inject(calls)] calls: &AtomicUsize, $parameter: $ty) -> (usize, String) {
                tokio::task::yield_now().await;
                calls.fetch_add(1, Ordering::Relaxed);
                ($marker, $parameter.to_string())
            }
        }
    };
}

fixture_api!(parcel_api, 0, "/svc/parcels/:parcel_id", parcel_id: i64);
fixture_api!(batch_entries_api, 1, "/svc/parcels/batch/:batch_id/entries", batch_id: i64);
fixture_api!(session_journal_api, 2, "/svc/internal/journal/session/:session_key", session_key: &str);
fixture_api!(page_text_api, 3, "/svc/library/pages/:page_key/text", page_key: &str);
fixture_api!(job_api, 4, "/svc/jobs/:job_id", job_id: i64);
fixture_api!(node_health_api, 5, "/svc/jobs/:job_id/remote-node/health", job_id: i64);
fixture_api!(node_files_api, 6, "/svc/jobs/:job_id/remote-node/files", job_id: u64);
fixture_api!(environment_api, 7, "/svc/jobs/:job_id/environment-check", job_id: i64);
fixture_api!(job_tools_api, 8, "/svc/jobs/:job_id/tools", job_id: i64);
fixture_api!(tool_package_api, 9, "/svc/v2/catalog/tools/:tool_id/package", tool_id: i32);
fixture_api!(event_api, 10, "/svc/v2/events/:event_key", event_key: &str);

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

// Synthetic equivalents of the production route-shape matrix. Literal names
// are deliberately opaque; `$` marks one nonempty captured segment.
const SHAPE_MATRIX: [(&str, usize); 28] = [
    ("/probe/a/$", 0),
    ("/probe/a/$/b", 1),
    ("/probe/a/$/c", 2),
    ("/probe/a/d/$/e", 3),
    ("/probe/f/g/a/$/h", 4),
    ("/probe/f/g/i/$", 5),
    ("/probe/f/j/$/k", 6),
    ("/probe/l/$", 7),
    ("/probe/l/$/m", 8),
    ("/probe/l/$/n", 9),
    ("/probe/o/p/$/q", 10),
    ("/probe/r/$", 11),
    ("/probe/r/$/s/t", 12),
    ("/probe/r/$/u", 13),
    ("/probe/r/$/v/w", 14),
    ("/probe/r/$/v/x", 15),
    ("/probe/r/$/v/y", 16),
    ("/probe/r/$/z", 17),
    ("/probe/r/$/j", 18),
    ("/probe/aa/$/j", 19),
    ("/probe/ab/ac/$/ad", 20),
    ("/probe/ab/ac/$/ae", 21),
    ("/probe/ab/ac/$/af", 22),
    ("/probe/ab/ac/$/ag", 23),
    ("/probe/ab/ac/$/ah", 24),
    ("/probe/ab/ai/j/$/b", 25),
    ("/probe/ab/aj/$", 26),
    ("/probe/ak/al/$/l", 27),
];

brz_http_server::registry!(group = shape_matrix, dependencies(calls: Arc<AtomicUsize>));

macro_rules! shape_route {
    ($name:ident, $marker:literal, $path:literal) => {
        #[brz_http_server::get($path, access = public, group = shape_matrix)]
        async fn $name(#[inject(calls)] calls: &AtomicUsize, value: &str) -> (usize, String) {
            calls.fetch_add(1, Ordering::Relaxed);
            ($marker, value.to_owned())
        }
    };
}

shape_route!(shape_00, 0, "/probe/a/:value");
shape_route!(shape_01, 1, "/probe/a/:value/b");
shape_route!(shape_02, 2, "/probe/a/:value/c");
shape_route!(shape_03, 3, "/probe/a/d/:value/e");
shape_route!(shape_04, 4, "/probe/f/g/a/:value/h");
shape_route!(shape_05, 5, "/probe/f/g/i/:value");
shape_route!(shape_06, 6, "/probe/f/j/:value/k");
shape_route!(shape_07, 7, "/probe/l/:value");
shape_route!(shape_08, 8, "/probe/l/:value/m");
shape_route!(shape_09, 9, "/probe/l/:value/n");
shape_route!(shape_10, 10, "/probe/o/p/:value/q");
shape_route!(shape_11, 11, "/probe/r/:value");
shape_route!(shape_12, 12, "/probe/r/:value/s/t");
shape_route!(shape_13, 13, "/probe/r/:value/u");
shape_route!(shape_14, 14, "/probe/r/:value/v/w");
shape_route!(shape_15, 15, "/probe/r/:value/v/x");
shape_route!(shape_16, 16, "/probe/r/:value/v/y");
shape_route!(shape_17, 17, "/probe/r/:value/z");
shape_route!(shape_18, 18, "/probe/r/:value/j");
shape_route!(shape_19, 19, "/probe/aa/:value/j");
shape_route!(shape_20, 20, "/probe/ab/ac/:value/ad");
shape_route!(shape_21, 21, "/probe/ab/ac/:value/ae");
shape_route!(shape_22, 22, "/probe/ab/ac/:value/af");
shape_route!(shape_23, 23, "/probe/ab/ac/:value/ag");
shape_route!(shape_24, 24, "/probe/ab/ac/:value/ah");
shape_route!(shape_25, 25, "/probe/ab/ai/j/:value/b");
shape_route!(shape_26, 26, "/probe/ab/aj/:value");
shape_route!(shape_27, 27, "/probe/ak/al/:value/l");

fn fixture_router(reverse: bool, calls: &Arc<AtomicUsize>) -> Router {
    let mut groups = vec![
        brz_http_server::handlers!(calls = calls.clone(); group = crate::parcel_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::batch_entries_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::session_journal_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::page_text_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::job_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::node_health_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::node_files_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::environment_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::job_tools_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::tool_package_api::http_apis).unwrap(),
        brz_http_server::handlers!(calls = calls.clone(); group = crate::event_api::http_apis).unwrap(),
    ];
    if reverse {
        groups.reverse();
    }
    groups.into_iter().fold(Router::default(), Router::merge)
}

struct Running {
    address: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<brz_http_server::Result<()>>,
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
async fn synthetic_shape_matrix_selects_every_dynamic_route() {
    let calls = Arc::new(AtomicUsize::new(0));
    let router = brz_http_server::handlers!(calls = calls.clone(); group = shape_matrix).unwrap();
    let server = Running::start(router).await;
    for (pattern, marker) in SHAPE_MATRIX {
        server
            .expect_capture(&pattern.replace('$', "plain-value"), marker, "plain-value")
            .await;
        server
            .expect_capture(&pattern.replace('$', "left%2Fright"), marker, "left/right")
            .await;
        let (status, headers, _) = server
            .request("DELETE", &pattern.replace('$', "plain-value"))
            .await;
        assert_eq!(status, 405, "{pattern}");
        assert!(
            headers.to_ascii_lowercase().contains("\r\nallow: get"),
            "{pattern}: {headers}"
        );
    }
    assert_eq!(server.request("GET", "/probe/r/v/v/none").await.0, 404);
    assert_eq!(calls.load(Ordering::Relaxed), SHAPE_MATRIX.len() * 2);
    server.stop().await;
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
            // Decode captures after raw slash boundaries are established, and
            // keep query slashes out of the path.
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

#[tokio::test]
async fn encoded_slashes_stay_inside_one_decoded_capture() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Running::start(fixture_router(false, &calls)).await;
    server
        .expect_capture("/svc/v2/events/a%2Fb", 10, "a/b")
        .await;
    server
        .expect_capture("/svc/v2/events/a%252Fb", 10, "a%2Fb")
        .await;
    assert_eq!(server.request("GET", "/svc/v2/events/a/b").await.0, 404);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    server.stop().await;
}

#[brz_http_server::get(
    "/svc/jobs/search",
    access = public,
    group = static_neighbors
)]
async fn search() -> &'static str {
    "static-search"
}

#[brz_http_server::get(
    "/svc/jobs/42/remote-node/health",
    access = public,
    group = static_neighbors
)]
async fn health() -> &'static str {
    "static-health"
}

#[brz_http_server::post(
    "/svc/jobs/42/tools",
    access = public,
    group = static_neighbors
)]
async fn tools() -> &'static str {
    "static-tools-post"
}

#[tokio::test]
async fn shared_prefixes_and_suffixes_do_not_steal_each_others_routes() {
    for static_first in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let dynamic = fixture_router(false, &calls);
        let router = if static_first {
            brz_http_server::handlers!(; group = static_neighbors)
                .unwrap()
                .merge(dynamic)
        } else {
            dynamic.merge(brz_http_server::handlers!(; group = static_neighbors).unwrap())
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
