#![cfg(feature = "hyper")]
use brz_http_server::{
    Handler, HeaderBlock, NoAuthenticator, Request, Response, ResponseStream, ServerConfig,
    StatusCode,
    hyper::{self, HandlerService},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};
struct Api(Arc<AtomicUsize>);
impl Handler for Api {
    async fn call(&self, request: Request<'_>, _: &NoAuthenticator) -> Response {
        if request.path() == "/stream" {
            let polls = self.0.clone();
            let source = futures_util::stream::iter(["one", "two"].map(move |value| {
                let polls = polls.clone();
                async move {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, std::io::Error>(brz_http_server::Bytes::from_static(value.as_bytes()))
                }
            }));
            let source = futures_util::StreamExt::then(source, |future| future);
            return Response::stream(
                StatusCode::OK,
                ResponseStream::new(source).with_content_length(6),
            );
        }
        let mut headers = request.response_bytes(64);
        headers.extend_from_slice(b"Set-Cookie: one=1\r\nSet-Cookie: two=2\r\n");
        Response::owned_bytes(StatusCode::OK, request.body().to_vec())
            .headers(HeaderBlock::new(headers.freeze()).unwrap())
    }
}
async fn head(client: &mut TcpStream) -> String {
    timeout(Duration::from_secs(3), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(client.read_u8().await.unwrap());
        }
        String::from_utf8(bytes).unwrap().to_ascii_lowercase()
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn native_adapter_preserves_body_pipeline_headers_head_stream_and_limits() {
    let polls = Arc::new(AtomicUsize::new(0));
    let service = HandlerService::new(
        Api(polls.clone()),
        ServerConfig {
            max_request_body_bytes: 4,
            max_in_flight_request_body_bytes: 4,
            ..Default::default()
        },
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(hyper::serve(
        listener,
        move |_| service.clone(),
        async {
            stopped.await.ok();
        },
        hyper::Config {
            shutdown_grace: Duration::from_millis(100),
            ..Default::default()
        },
    ));
    let mut client = TcpStream::connect(address).await.unwrap();
    client.write_all(b"POST /echo HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\n\r\ndataHEAD /stream HTTP/1.1\r\nHost: test\r\n\r\nGET /stream HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").await.unwrap();
    let echo = head(&mut client).await;
    assert!(echo.starts_with("http/1.1 200"));
    assert!(echo.contains("set-cookie: one=1\r\n"));
    assert!(echo.contains("set-cookie: two=2\r\n"));
    let mut bytes = [0; 4];
    client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"data");
    let headers = head(&mut client).await;
    assert!(headers.starts_with("http/1.1 200"));
    assert!(headers.contains("content-length: 6\r\n"));
    let stream = head(&mut client).await;
    assert!(stream.starts_with("http/1.1 200"));
    let mut body = Vec::new();
    timeout(Duration::from_secs(3), client.read_to_end(&mut body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, b"onetwo");
    assert_eq!(
        polls.load(Ordering::SeqCst),
        2,
        "HEAD must not poll its stream"
    );
    let mut oversized = TcpStream::connect(address).await.unwrap();
    oversized
        .write_all(b"POST /echo HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\n12345")
        .await
        .unwrap();
    assert!(head(&mut oversized).await.starts_with("http/1.1 413"));
    stop.send(()).unwrap();
    timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
