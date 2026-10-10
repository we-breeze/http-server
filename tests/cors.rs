use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use brz_http_server::{
    Cors, Handler, HeaderBlock, NoAuthenticator, Request, Response, Server, ServerConfig,
    StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

struct Probe(Arc<AtomicUsize>);

impl Handler for Probe {
    async fn call<'a>(&'a self, request: Request<'a>, _: &'a NoAuthenticator) -> Response {
        self.0.fetch_add(1, Ordering::SeqCst);
        let mut bytes = request.response_bytes(256);
        bytes.write_all(b"Vary: Accept-Encoding\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nAccess-Control-Allow-Origin: *\r\n").unwrap();
        Response::static_bytes(StatusCode::OK, b"handler")
            .headers(HeaderBlock::new(bytes.freeze()).unwrap())
    }
}

async fn request(cors: Option<Cors>, head: &str) -> (String, usize) {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::bind_with_config(
        "127.0.0.1:0".parse().unwrap(),
        Probe(Arc::clone(&calls)),
        ServerConfig {
            cors,
            ..ServerConfig::default()
        },
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    let mut client = TcpStream::connect(address).await.unwrap();
    client
        .write_all(format!("{head}\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    (
        String::from_utf8(response).unwrap(),
        calls.load(Ordering::SeqCst),
    )
}

fn policy() -> Cors {
    Cors {
        allow_credentials: true,
        expose_headers: vec!["X-Request-ID".into()],
        extra_preflight_vary: vec!["X-Preflight-Variant".into()],
        ..Cors::permissive()
    }
}

fn values<'a>(response: &'a str, name: &str) -> Vec<&'a str> {
    response
        .split_once("\r\n\r\n")
        .unwrap()
        .0
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .collect()
}

#[tokio::test]
async fn shared_preflight_runs_before_the_handler() {
    let (response, calls) = request(Some(policy()), "OPTIONS /missing HTTP/1.1\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: GET\r\nAccess-Control-Request-Headers: authorization").await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.ends_with("OK"));
    assert_eq!(calls, 0);
    assert!(values(&response, "access-control-expose-headers").is_empty());
    assert_eq!(
        values(&response, "vary"),
        [
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers, X-Preflight-Variant"
        ]
    );
    assert_eq!(
        values(&response, "access-control-allow-origin"),
        ["https://app.example"]
    );
    assert_eq!(
        values(&response, "access-control-allow-headers"),
        ["authorization"]
    );
}

#[tokio::test]
async fn actual_response_merges_vary_and_replaces_cors_without_losing_cookies() {
    let (response, calls) = request(
        Some(policy()),
        "GET / HTTP/1.1\r\nOrigin: https://app.example",
    )
    .await;
    assert!(response.ends_with("handler"));
    assert_eq!(calls, 1);
    assert_eq!(
        values(&response, "access-control-allow-origin"),
        ["https://app.example"]
    );
    assert_eq!(values(&response, "vary"), ["Accept-Encoding", "Origin"]);
    assert_eq!(
        values(&response, "access-control-expose-headers"),
        ["X-Request-ID"]
    );
    assert_eq!(values(&response, "set-cookie"), ["a=1", "b=2"]);
}

#[tokio::test]
async fn responses_without_origin_merge_vary_only_when_cors_is_enabled() {
    for enabled in [true, false] {
        let (response, calls) = request(enabled.then(policy), "GET / HTTP/1.1").await;
        assert!(response.ends_with("handler"));
        assert_eq!(calls, 1);
        let expected = if enabled {
            vec!["Accept-Encoding", "Origin"]
        } else {
            vec!["Accept-Encoding"]
        };
        assert_eq!(values(&response, "vary"), expected);
        assert_eq!(values(&response, "set-cookie"), ["a=1", "b=2"]);
        assert!(values(&response, "access-control-allow-credentials").is_empty());
        assert!(values(&response, "access-control-expose-headers").is_empty());
    }
}

#[tokio::test]
async fn plain_options_and_disabled_cors_reach_the_handler() {
    for (cors, head) in [
        (
            Some(policy()),
            "OPTIONS / HTTP/1.1\r\nOrigin: https://app.example",
        ),
        (
            None,
            "OPTIONS / HTTP/1.1\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: GET",
        ),
    ] {
        let (response, calls) = request(cors, head).await;
        assert!(response.ends_with("handler"));
        assert_eq!(calls, 1);
    }
}
