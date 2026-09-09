#![cfg(feature = "macros")]

use brz_http_server::{Server, Text};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

brz_http_server::registry!();

#[brz_http_server::get("/headers/:id")]
async fn headers(
    #[header] required: u32,
    #[header("authorization")] token: Option<&str>,
    id: u32,
    #[header] x_api_key: Option<&str>,
    #[header] r#type: Option<&str>,
) -> Text {
    tokio::task::yield_now().await;
    Text(format!("{id}:{required}:{token:?}:{x_api_key:?}:{type:?}"))
}

async fn request(address: std::net::SocketAddr, path: &str, headers: &str) -> String {
    let mut client = TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn header_attributes_bind_names_types_and_optional_values() {
    assert_eq!(
        headers(7, None, 42, None, None).await.0,
        "42:7:None:None:None"
    );
    let server = Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        brz_http_server::handlers!().unwrap(),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(async {
        let _ = rx.await;
    }));
    for (path, headers, expected) in [
        (
            "/headers/42",
            "Required: 7\r\nAuthorization: bearer\r\nx_api_key: literal\r\nx-api-key: hyphenated\r\nType: raw\r\n",
            "42:7:Some(\"bearer\"):Some(\"literal\"):Some(\"raw\")",
        ),
        (
            "/headers/42?token=query&x_api_key=query",
            "Required: 7\r\nx-api-key: hyphenated\r\n",
            "42:7:None:None:None",
        ),
    ] {
        let response = request(address, path, headers).await;
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
        assert_eq!(response.split_once("\r\n\r\n").unwrap().1, expected);
    }
    for headers in ["", "Required: invalid\r\n"] {
        let response = request(address, "/headers/42?required=7", headers).await;
        assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
    }
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}
