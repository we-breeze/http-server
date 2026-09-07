#![cfg(feature = "macros")]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use http_server::{FromState, Server, api};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

struct Payload {
    value: u32,
}

#[derive(FromState)]
struct SharedApi<T>
where
    T: Send + Sync,
{
    state: Arc<T>,
}

trait Provider {
    type State;
}

struct SharedProvider;
impl Provider for SharedProvider {
    type State = Arc<Payload>;
}

#[derive(http_server::FromState)]
struct ProjectedApi<T: Provider> {
    state: T::State,
}

#[derive(http_server::FromState)]
struct BorrowedApi<'a, const N: usize> {
    state: &'a [u8; N],
}

#[test]
fn preserves_generic_bounds_associated_types_and_borrowed_state() {
    // Payload has no Clone implementation; only Arc<Payload> must be cloned.
    let state = Arc::new(Payload { value: 42 });
    let shared = SharedApi::from_state(&state);
    let projected = ProjectedApi::<SharedProvider>::from_state(&state);
    assert!(Arc::ptr_eq(&shared.state, &state));
    assert!(Arc::ptr_eq(&projected.state, &state));
    assert_eq!(shared.state.value, 42);
    assert_eq!(Arc::strong_count(&state), 3);

    let data = [1, 2, 3];
    let borrowed = BorrowedApi::from_state(&&data);
    assert!(std::ptr::eq(borrowed.state, &raw const data));
}

struct CloneState {
    clones: Arc<AtomicUsize>,
    value: u32,
}

impl Clone for CloneState {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            clones: Arc::clone(&self.clones),
            value: self.value,
        }
    }
}

impl CloneState {
    #[allow(dead_code, clippy::unused_self)]
    fn clone(&self) -> Self {
        panic!("derive must call the Clone trait, not this inherent method")
    }
}

http_server::registry!(state = CloneState);

mod endpoints {
    use super::{CloneState, api};

    #[derive(http_server::FromState)]
    struct DerivedApi {
        state: CloneState,
    }

    #[api(prefix = "/fixture")]
    impl DerivedApi {
        #[http_server::get("/derived/:offset")]
        async fn read(&self, offset: u32) -> u32 {
            tokio::task::yield_now().await;
            self.state.value + offset
        }
    }
}

#[tokio::test]
async fn derived_private_field_registers_and_clones_only_when_collecting() {
    let clones = Arc::new(AtomicUsize::new(0));
    let state = CloneState {
        clones: Arc::clone(&clones),
        value: 40,
    };
    let handler = http_server::handlers!(state).unwrap();
    assert_eq!(clones.load(Ordering::Relaxed), 1);
    let server = Server::bind("127.0.0.1:0".parse().unwrap(), handler)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    for offset in [1, 2, 3] {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(format!("GET /fixture/derived/{offset} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 "), "{response}");
        assert_eq!(body, (40 + offset).to_string());
    }
    assert_eq!(clones.load(Ordering::Relaxed), 1);
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}
