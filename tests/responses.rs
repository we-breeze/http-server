#![cfg(feature = "macros")]

use brz_http_server::{Handler, Redirect, Server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
};

#[allow(clippy::needless_pass_by_value)]
async fn request(handler: impl Handler, request: &str) -> Vec<u8> {
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), handler)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    let _ = tx.send(());
    task.await.unwrap().unwrap();
    out
}
#[brz_http_server::get("/callback")]
async fn callback(redirect: &str) -> Redirect {
    Redirect::found(redirect)
}
#[tokio::test]
async fn redirect_ascii_works() {
    let response = request(brz_http_server::handlers!().unwrap(), "GET /callback?redirect=%2Flogin%3Fsuccess%3Dtrue HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").await;
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 302 Found\r\n"), "{response}");
    assert!(
        response.contains("location: /login?success=true\r\n"),
        "{response}"
    );
    assert!(response.ends_with("\r\n\r\n"));
}
#[tokio::test]
async fn redirect_encodes_source_error_message() {
    let response = request(brz_http_server::handlers!().unwrap(), "GET /callback?redirect=%2Flogin%3Fmessage%3Dbad+token HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").await;
    let response = String::from_utf8(response).unwrap();
    assert!(
        response.contains("location: /login?message=bad%20token\r\n"),
        "{response}"
    );
}
#[brz_http_server::get("/items")]
async fn list() -> u64 {
    1
}
#[brz_http_server::post("/items")]
async fn create() -> u64 {
    2
}
#[tokio::test]
async fn merged_same_path_routes_by_method() {
    let response = request(
        brz_http_server::handlers!().unwrap(),
        "POST /items HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with('2'));
}
brz_http_server::registry!();

type HttpStatus = http::StatusCode;
type FailedPayload = (HttpStatus, std::collections::HashMap<Vec<u8>, i64>);
#[brz_http_server::get("/invalid")]
async fn invalid() -> FailedPayload {
    (
        http::StatusCode::CREATED,
        [(vec![1, 2], 42)].into_iter().collect(),
    )
}
#[tokio::test]
async fn serialization_failure_is_500() {
    let response = request(
        brz_http_server::handlers!().unwrap(),
        "GET /invalid HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
    )
    .await;
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 500 "), "{response}");
}
