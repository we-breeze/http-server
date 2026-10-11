use brz_http_server::{
    Cors, Handler, NoAuthenticator, Request, Response, Server, ServerConfig, StatusCode,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
};

struct Errors;
impl Handler for Errors {
    async fn call<'a>(&'a self, request: Request<'a>, _: &'a NoAuthenticator) -> Response {
        let response = Response::static_bytes(StatusCode::INTERNAL_SERVER_ERROR, b"error")
            .content_type("application/json");
        if request.path() == "/unhandled" {
            response.without_cors()
        } else {
            response
        }
    }
}

#[tokio::test]
async fn public_skip_stays_local_and_internal_export_is_explicit() {
    for export in [false, true] {
        let server = Server::bind_with_config(
            "127.0.0.1:0".parse().unwrap(),
            Errors,
            ServerConfig {
                cors: Some(Cors {
                    allow_credentials: true,
                    ..Cors::permissive()
                }),
                export_cors_policy: export,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let address = server.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.serve_until(async {
            let _ = stopped.await;
        }));
        for origin in ["", "Origin: https://app.example\r\n"] {
            for path in ["/unhandled", "/business"] {
                let mut client = TcpStream::connect(address).await.unwrap();
                client.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{origin}Connection: close\r\n\r\n").as_bytes()).await.unwrap();
                let mut response = Vec::new();
                tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
                    .await
                    .unwrap()
                    .unwrap();
                let response = String::from_utf8(response).unwrap();
                let lower = response.to_ascii_lowercase();
                assert!(response.starts_with("HTTP/1.1 500"));
                assert!(response.ends_with("error"));
                assert_eq!(
                    lower.contains("x-breeze-response-cors: skip"),
                    export && path == "/unhandled"
                );
                assert_eq!(lower.contains("vary: origin"), path == "/business");
                assert_eq!(
                    lower.contains("access-control-allow-origin:"),
                    path == "/business" && !origin.is_empty()
                );
            }
        }
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
