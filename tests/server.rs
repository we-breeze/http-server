use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use http_server::{
    EphemeralBytesArena, Handler, HeaderBlock, NoAuthenticator, Request, Response, Server,
    ServerConfig, StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

struct Echo {
    response_used_arena: Arc<AtomicBool>,
}

impl Handler for Echo {
    async fn call(&self, request: Request<'_>, _: &NoAuthenticator) -> Response {
        match (request.method(), request.path()) {
            ("POST", "/echo") => {
                assert_eq!(request.query(), Some("request=one"));
                assert_eq!(request.header("host"), Some(&b"localhost"[..]));

                let mut headers = request.response_bytes(15);
                headers.extend_from_slice(b"X-Route: echo\r\n");
                let headers = HeaderBlock::new(headers.freeze()).unwrap();

                let mut body = request.response_bytes(request.body().len());
                body.extend_from_slice(request.body());
                self.response_used_arena
                    .store(!body.is_heap_allocated(), Ordering::Release);
                Response::ok(body.freeze())
                    .headers(headers)
                    .content_type("application/octet-stream")
            }
            ("GET", "/empty") => Response::static_bytes(StatusCode::OK, b"done"),
            _ => Response::empty(StatusCode::new(404)),
        }
    }
}

fn start_server<H>(server: Server<H>) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>)
where
    H: Handler,
{
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .serve_until(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (shutdown_tx, task)
}

async fn read_to_close(stream: &mut TcpStream) -> Vec<u8> {
    let mut output = Vec::new();
    stream.read_to_end(&mut output).await.unwrap();
    output
}

#[tokio::test]
async fn borrowed_request_and_arena_body_support_keep_alive_and_pipelining() {
    let arena = EphemeralBytesArena::new(1024);
    let response_used_arena = Arc::new(AtomicBool::new(false));
    let server = Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        Echo {
            response_used_arena: Arc::clone(&response_used_arena),
        },
        ServerConfig::new(arena),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown, task) = start_server(server);

    let mut client = TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            b"POST /echo?request=one HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello\
              GET /empty HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let response = read_to_close(&mut client).await;

    assert_eq!(
        response,
        b"HTTP/1.1 200 OK\r\nX-Route: echo\r\nContent-Type: application/octet-stream\r\nContent-Length: 5\r\n\r\nhello\
          HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone"
    );
    assert!(response_used_arena.load(Ordering::Acquire));

    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn rejects_a_body_over_the_configured_limit_before_calling_handler() {
    let unused_handler = Echo {
        response_used_arena: Arc::new(AtomicBool::new(false)),
    };
    let arena = EphemeralBytesArena::new(1024);
    let mut config = ServerConfig::new(arena);
    config.max_request_body_bytes = 4;
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), unused_handler, config)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown, task) = start_server(server);

    let mut client = TcpStream::connect(address).await.unwrap();
    client
        .write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\n")
        .await
        .unwrap();
    let response = read_to_close(&mut client).await;
    assert_eq!(
        response,
        b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );

    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn rejects_excess_connections_without_stalling_active_connections() {
    let arena = EphemeralBytesArena::new(1024);
    let mut config = ServerConfig::new(arena);
    config.max_connections = 1;
    let server = Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        Echo {
            response_used_arena: Arc::new(AtomicBool::new(false)),
        },
        config,
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown, task) = start_server(server);

    let first = TcpStream::connect(address).await.unwrap();
    let mut second = TcpStream::connect(address).await.unwrap();
    let mut byte = [0_u8; 1];
    assert_eq!(second.read(&mut byte).await.unwrap(), 0);

    drop(first);
    drop(second);
    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn bind_rejects_invalid_limits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let arena = EphemeralBytesArena::new(128);
    let mut config = ServerConfig::new(arena);
    config.max_connections = 0;

    let result = Server::bind(
        address,
        Echo {
            response_used_arena: Arc::new(AtomicBool::new(false)),
        },
        config,
    )
    .await;
    let Err(error) = result else {
        panic!("zero max_connections must fail validation");
    };
    assert_eq!(
        error.to_string(),
        "invalid server configuration: max_connections must be greater than zero"
    );
}
