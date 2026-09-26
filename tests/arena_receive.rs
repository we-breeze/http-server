//! Network regressions for the arena receive path (no throughput claims).
use brz_http_server::{
    Handler, NoAuthenticator, Request, Response, Server, ServerConfig, StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct CheckBody;
impl Handler for CheckBody {
    async fn call(&self, request: Request<'_>, _: &NoAuthenticator) -> Response {
        match request.path() {
            "/post" => {
                assert_eq!(request.body_view().len(), 5000);
                let view = request.body_view();
                let mut offset = 0;
                while offset < view.len() {
                    let bytes = view.chunk_at(offset);
                    assert!(!bytes.is_empty());
                    assert!(bytes.iter().all(|b| *b == b'x'));
                    offset += bytes.len();
                }
                assert_eq!(view.peek_byte(5000), None);
            }
            "/next" => assert_eq!(request.body_view().len(), 0),
            _ => panic!("unexpected route"),
        }
        Response::static_bytes(StatusCode::OK, b"OK")
    }
}

#[tokio::test]
async fn fragmented_body_and_pipelined_next_request_are_not_mixed() {
    for initial in [64, 2048, 4096] {
        let config = ServerConfig {
            initial_request_segment_bytes: initial,
            ..ServerConfig::default()
        };
        let server = Server::bind_with_config("127.0.0.1:0".parse().unwrap(), CheckBody, config)
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.serve_until(async {
            let _ = stopped.await;
        }));
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut wire = b"POST /post HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5000\r\nX-Request-Id: test-id\r\n\r\n".to_vec();
        wire.extend_from_slice(&[b'x'; 5000]);
        wire.extend_from_slice(b"GET /next HTTP/1.1\r\nConnection: close\r\n\r\n");
        for bytes in wire.chunks(317) {
            client.write_all(bytes).await.unwrap();
        }
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let text = String::from_utf8(response).unwrap();
        assert_eq!(text.matches("HTTP/1.1 200").count(), 2, "{text}");
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn malformed_partial_head_is_rejected_before_the_request_timeout() {
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), CheckBody)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    let mut client = TcpStream::connect(address).await.unwrap();
    client.write_all(b"\0").await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        client.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}
