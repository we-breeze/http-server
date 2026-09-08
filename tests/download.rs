#![cfg(feature = "macros")]

use std::convert::Infallible;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use brz_http_server::{
    Bytes, Handler, HttpResponse, IntoHttpResponse, NoAuthenticator, Request, Response, Server,
    ServerConfig, StatusCode, Stream, api,
};
use futures_util::stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Notify, oneshot};

struct Downloads;
#[api(register = false)]
impl Downloads {
    #[brz_http_server::get("/file")]
    async fn file(&self) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
        stream::iter([
            Ok(Bytes::from_static(b"hello")),
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"\0\xffworld")),
        ])
    }
    #[brz_http_server::get("/range")]
    async fn range(&self) -> HttpResponse<impl Stream<Item = io::Result<Bytes>> + Send + 'static> {
        HttpResponse::new(stream::iter([Ok(Bytes::from_static(b"xyz"))]))
            .status(StatusCode::PARTIAL_CONTENT)
            .content_length(3)
            .header("content-type", "video/mp4")
            .unwrap()
            .header("content-range", "bytes 2-4/9")
            .unwrap()
            .header("content-disposition", "attachment; filename=video.mp4")
            .unwrap()
    }
    #[brz_http_server::get("/short")]
    async fn short(&self) -> HttpResponse<impl Stream<Item = io::Result<Bytes>> + Send + 'static> {
        HttpResponse::new(stream::iter([Ok(Bytes::from_static(b"abc"))])).content_length(4)
    }
    #[brz_http_server::get("/long")]
    async fn long(&self) -> HttpResponse<impl Stream<Item = io::Result<Bytes>> + Send + 'static> {
        HttpResponse::new(stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"overflow")),
        ]))
        .content_length(4)
    }
    #[brz_http_server::get("/error")]
    async fn error(&self) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
        stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Err(io::Error::other("upstream failed")),
        ])
    }
    #[brz_http_server::get("/empty")]
    async fn empty(&self) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
        stream::empty()
    }
}

async fn start<H: Handler>(
    handler: H,
) -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), handler)
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .serve_until(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx, task)
}

async fn exchange(path: &str) -> Vec<u8> {
    let (addr, tx, task) = start(Downloads).await;
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut wire = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut wire))
        .await
        .unwrap()
        .unwrap();
    tx.send(()).unwrap();
    task.await.unwrap();
    wire
}

fn split_response(wire: &[u8]) -> (&str, &[u8]) {
    let offset = wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (
        std::str::from_utf8(&wire[..offset]).unwrap(),
        &wire[offset..],
    )
}

#[tokio::test]
async fn direct_stream_uses_chunked_encoding_and_preserves_binary_bytes() {
    let wire = exchange("/file").await;
    let (head, body) = split_response(&wire);
    assert!(head.contains("Transfer-Encoding: chunked\r\n"));
    assert!(!head.contains("Content-Length:"));
    assert_eq!(body, b"5\r\nhello\r\n7\r\n\0\xffworld\r\n0\r\n\r\n");
    assert_eq!(split_response(&exchange("/empty").await).1, b"0\r\n\r\n");
}

#[tokio::test]
async fn known_length_range_preserves_metadata_without_chunked_encoding() {
    let wire = exchange("/range").await;
    let (head, body) = split_response(&wire);
    assert!(head.starts_with("HTTP/1.1 206 Partial Content\r\n"));
    assert!(head.contains("Content-Length: 3\r\n"));
    assert!(head.contains("content-range: bytes 2-4/9\r\n"));
    assert!(head.contains("content-type: video/mp4\r\n"));
    assert!(head.contains("content-disposition: attachment; filename=video.mp4\r\n"));
    assert!(!head.contains("Transfer-Encoding:"));
    assert_eq!(body, b"xyz");
}

#[tokio::test]
async fn upstream_error_and_length_mismatch_close_without_faking_completion() {
    let wire = exchange("/error").await;
    assert_eq!(split_response(&wire).1, b"3\r\nabc\r\n");
    for path in ["/short", "/long"] {
        let wire = exchange(path).await;
        let (head, body) = split_response(&wire);
        assert!(head.contains("Content-Length: 4\r\n"));
        assert_eq!(body, b"abc");
    }
}

struct Gated {
    release: Arc<Notify>,
    dropped: Arc<Notify>,
    polls: Arc<AtomicUsize>,
}
struct DropSignal(Arc<Notify>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
impl Handler for Gated {
    async fn call(&self, request: Request<'_>, _: &NoAuthenticator) -> Response {
        let release = self.release.clone();
        let polls = self.polls.clone();
        let guard = DropSignal(self.dropped.clone());
        let stream = stream::unfold((0, guard), move |(index, guard)| {
            let release = release.clone();
            let polls = polls.clone();
            async move {
                polls.fetch_add(1, Ordering::SeqCst);
                if index > 0 {
                    release.notified().await;
                }
                Some((
                    Ok::<_, io::Error>(Bytes::from_static(b"first")),
                    (index + 1, guard),
                ))
            }
        });
        let status = if request.path() == "/no-content" {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::OK
        };
        (status, stream).into_http_response(request.response_arena())
    }
}

#[tokio::test]
async fn first_chunk_arrives_before_upstream_finishes_and_timeout_drops_upstream() {
    let release = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let polls = Arc::new(AtomicUsize::new(0));
    let config = ServerConfig {
        request_timeout: Duration::from_millis(200),
        ..ServerConfig::default()
    };
    let server = Server::bind_with_config(
        "127.0.0.1:0".parse().unwrap(),
        Gated {
            release,
            dropped: dropped.clone(),
            polls: polls.clone(),
        },
        config,
    )
    .await
    .unwrap();
    let addr = server.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !received.ends_with(b"5\r\nfirst\r\n") {
            let n = socket.read_buf(&mut received).await.unwrap();
            assert!(n > 0);
        }
    })
    .await
    .unwrap();
    // The upstream is still waiting; the first chunk has already arrived.
    tokio::time::timeout(Duration::from_secs(2), dropped.notified())
        .await
        .unwrap();
    socket.read_to_end(&mut received).await.unwrap();
    assert!(!received.ends_with(b"0\r\n\r\n"));
    assert!(polls.load(Ordering::SeqCst) <= 2);
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn head_and_no_content_do_not_poll_upstream() {
    for (method, path) in [("HEAD", "/"), ("GET", "/no-content")] {
        let dropped = Arc::new(Notify::new());
        let polls = Arc::new(AtomicUsize::new(0));
        let (addr, tx, task) = start(Gated {
            release: Arc::new(Notify::new()),
            dropped: dropped.clone(),
            polls: polls.clone(),
        })
        .await;
        let mut socket = TcpStream::connect(addr).await.unwrap();
        socket
            .write_all(
                format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut wire = Vec::new();
        socket.read_to_end(&mut wire).await.unwrap();
        assert_eq!(split_response(&wire).1, b"");
        tokio::time::timeout(Duration::from_secs(2), dropped.notified())
            .await
            .unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        tx.send(()).unwrap();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn chunked_response_completes_before_next_pipelined_response() {
    let (addr, tx, task) = start(Downloads).await;
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket.write_all(b"GET /empty HTTP/1.1\r\nHost: test\r\n\r\nGET /range HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut wire = Vec::new();
    socket.read_to_end(&mut wire).await.unwrap();
    assert!(String::from_utf8_lossy(&wire).contains("0\r\n\r\nHTTP/1.1 206 Partial Content\r\n"));
    tx.send(()).unwrap();
    task.await.unwrap();
}

struct Continuous(Arc<Notify>);
impl Handler for Continuous {
    async fn call(&self, request: Request<'_>, _: &NoAuthenticator) -> Response {
        let guard = DropSignal(self.0.clone());
        let chunk = Bytes::from(vec![7; 128 * 1024]);
        stream::unfold((guard, chunk), |(guard, chunk)| async move {
            Some((Ok::<_, io::Error>(chunk.clone()), (guard, chunk)))
        })
        .into_http_response(request.response_arena())
    }
}

#[tokio::test]
async fn client_disconnect_cancels_the_upstream_producer() {
    let dropped = Arc::new(Notify::new());
    let (addr, tx, task) = start(Continuous(dropped.clone())).await;
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut first = [0; 64];
    socket.read_exact(&mut first).await.unwrap();
    drop(socket);
    tokio::time::timeout(Duration::from_secs(2), dropped.notified())
        .await
        .unwrap();
    tx.send(()).unwrap();
    task.await.unwrap();
}
