use brz_http_server::{
    Handler, NoAuthenticator, Request, Response, Server, ServerConfig, StatusCode, Upgrade,
};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
const DEADLINE: Duration = Duration::from_secs(3);
struct Protocol(Arc<AtomicUsize>);
impl Handler for Protocol {
    async fn call(&self, request: Request<'_>, _: &NoAuthenticator) -> Response {
        if request.path() == "/bare" {
            return Response::empty(StatusCode::SWITCHING_PROTOCOLS);
        }
        let entered = self.0.clone();
        Upgrade::accept(&request, "test", move |mut io| async move {
            entered.fetch_add(1, Ordering::SeqCst);
            let mut first = [0; 5];
            io.read_exact(&mut first).await?;
            // Upgrade callbacks must outlive the ordinary request deadline.
            tokio::time::sleep(Duration::from_millis(120)).await;
            io.write_all(&first).await?;
            let (mut read, mut write) = tokio::io::split(io);
            tokio::io::copy(&mut read, &mut write).await?;
            Ok(())
        })
        .unwrap_or_else(|_| Response::empty(StatusCode::BAD_REQUEST).close())
    }
}
async fn head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    timeout(DEADLINE, async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
            assert!(head.len() < 8192);
        }
    })
    .await
    .unwrap();
    String::from_utf8(head).unwrap()
}
async fn start(
    compat: bool,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<()>,
    Arc<AtomicUsize>,
) {
    let count = Arc::new(AtomicUsize::new(0));
    let config = ServerConfig {
        max_connections: 1,
        request_timeout: Duration::from_millis(80),
        shutdown_grace: Duration::from_millis(100),
        ..Default::default()
    };
    let (stop, stopped) = oneshot::channel();
    #[cfg(feature = "hyper")]
    if compat {
        let service =
            brz_http_server::hyper::HandlerService::new(Protocol(count.clone()), config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            brz_http_server::hyper::serve(
                listener,
                move |_| service.clone(),
                async {
                    stopped.await.ok();
                },
                brz_http_server::hyper::Config {
                    max_connections: 1,
                    shutdown_grace: Duration::from_millis(100),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        });
        return (address, stop, task, count);
    }
    let _ = compat;
    let server = Server::bind_with_config(
        "127.0.0.1:0".parse().unwrap(),
        Protocol(count.clone()),
        config,
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(async {
        server
            .serve_until(async {
                stopped.await.ok();
            })
            .await
            .unwrap();
    });
    (address, stop, task, count)
}
async fn verify_handoff(compat: bool) {
    let (address, stop, task, count) = start(compat).await;
    let mut client = TcpStream::connect(address).await.unwrap();
    // Body and first protocol bytes arrive together with the HTTP head.
    client.write_all(b"GET /upgrade HTTP/1.1\r\nHost: test\r\nConnection: keep-alive, Upgrade\r\nUpgrade: test\r\nContent-Length: 4\r\n\r\nbodyFIRST").await.unwrap();
    let response = head(&mut client).await.to_ascii_lowercase();
    assert!(response.starts_with("http/1.1 101"));
    assert!(response.contains("connection: upgrade\r\n"));
    assert!(!response.contains("content-length"));
    assert!(!response.contains("transfer-encoding"));
    let mut first = [0; 5];
    timeout(DEADLINE, client.read_exact(&mut first))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&first, b"FIRST");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    // Upgraded connections still consume the listener's admission budget.
    let mut extra = TcpStream::connect(address).await.unwrap();
    let mut byte = [0];
    let refused = timeout(DEADLINE, extra.read(&mut byte)).await.unwrap();
    assert!(matches!(refused, Ok(0)) || refused.is_err());
    stop.send(()).unwrap();
    timeout(DEADLINE, task).await.unwrap().unwrap();
    assert_eq!(
        timeout(DEADLINE, client.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn native_handoff_preserves_read_ahead_and_lifetime() {
    verify_handoff(false).await;
}
#[cfg(feature = "hyper")]
#[tokio::test]
async fn compatibility_handoff_preserves_read_ahead_and_lifetime() {
    verify_handoff(true).await;
}
#[tokio::test]
async fn rejected_and_bare_101_responses_never_enter_protocol_handler() {
    for (path, headers, expected) in [
        ("/upgrade", "", "400"),
        ("/bare", "Connection: Upgrade\r\nUpgrade: test\r\n", "500"),
    ] {
        let (address, stop, task, count) = start(false).await;
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: test\r\n{headers}\r\n").as_bytes())
            .await
            .unwrap();
        assert!(
            head(&mut client)
                .await
                .starts_with(&format!("HTTP/1.1 {expected}"))
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        stop.send(()).unwrap();
        timeout(DEADLINE, task).await.unwrap().unwrap();
    }
}
