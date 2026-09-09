#![cfg(feature = "macros")]

use brz_http_server::{Router, Server, Text};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

brz_http_server::registry!(dependencies(label: &'static str));
brz_http_server::registry!(group = runtime_check, dependencies(label: &'static str));
brz_http_server::registry!(group = r#type, dependencies(label: &'static str));

mod http_apis {
    use super::Text;

    #[brz_http_server::get("/value")]
    async fn value(#[inject(label)] label: &str) -> Text {
        Text(label.into())
    }
}

mod runtime_check {
    use super::Text;

    #[brz_http_server::get("/value", group = runtime_check)]
    async fn value(#[inject(label)] label: &str) -> Text {
        Text(label.into())
    }
}

mod r#type {
    use super::Text;

    #[brz_http_server::get("/value", group = r#type)]
    async fn value(#[inject(label)] label: &str) -> Text {
        Text(label.into())
    }
}

mod listeners {
    use super::{Router, Text};

    brz_http_server::registry!(group = runtime_check, dependencies(label: &'static str));

    mod runtime_check {
        use super::Text;

        #[brz_http_server::get("/value", group = super::runtime_check)]
        async fn value(#[inject(label)] label: &str) -> Text {
            Text(label.into())
        }
    }

    pub fn router() -> Router {
        brz_http_server::handlers!(label = "relative"; group = self::runtime_check).unwrap()
    }
}

#[tokio::test]
async fn logical_groups_coexist_with_business_modules_and_remain_isolated() {
    for (router, expected) in [
        (brz_http_server::handlers!(label = "default").unwrap(), "default"),
        (brz_http_server::handlers!(label = "explicit default"; group = crate::http_apis).unwrap(), "explicit default"),
        (brz_http_server::handlers!(label = "root"; group = runtime_check).unwrap(), "root"),
        (brz_http_server::handlers!(label = "raw"; group = r#type).unwrap(), "raw"),
        (brz_http_server::handlers!(label = "qualified"; group = crate::listeners::runtime_check).unwrap(), "qualified"),
        (listeners::router(), "relative"),
    ] {
        let server = Server::bind("127.0.0.1:0".parse().unwrap(), router).await.unwrap();
        let address = server.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.serve_until(async { let _ = rx.await; }));
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"GET /value HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
        assert_eq!(response.split_once("\r\n\r\n").unwrap().1, expected);
        tx.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
