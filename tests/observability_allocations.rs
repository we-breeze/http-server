//! Allocation assertions for the changed helpers, not an end-to-end HTTP claim.
//! HTTP production code adds no unsafe (the receive boundary is in brz-ds).
//! This isolated test allocator forwards each
//! allocation to System and counts only the current test thread's measured scope.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
#[cfg(any(feature = "api-log", feature = "slow-log"))]
use std::fmt::{self, Write as _};

use brz_http_server::__private::QueryParams;

#[cfg(any(feature = "api-log", feature = "slow-log"))]
#[allow(dead_code)]
#[path = "../src/api_metrics/logging.rs"]
mod logging;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

fn allocated() {
    let _ = ALLOCATIONS.try_with(|slot| {
        if let Some(count) = slot.get() {
            slot.set(Some(count + 1));
        }
    });
}

struct Counted;
// SAFETY: All operations preserve their arguments and forward ownership to the
// same System allocator. The counter never allocates and tolerates TLS teardown.
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated();
        unsafe { System.realloc(pointer, layout, size) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counted = Counted;

fn count<T>(work: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ALLOCATIONS.with(|slot| slot.set(None));
        }
    }
    ALLOCATIONS.with(|slot| {
        assert!(slot.get().is_none(), "measurement must not be nested");
        slot.set(Some(0));
    });
    let reset = Reset;
    let value = work();
    let allocations = ALLOCATIONS.with(|slot| slot.get().unwrap());
    drop(reset);
    (value, allocations)
}

#[cfg(any(feature = "api-log", feature = "slow-log"))]
#[derive(Default)]
struct Sink(usize);
#[cfg(any(feature = "api-log", feature = "slow-log"))]
impl fmt::Write for Sink {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0 += text.len();
        Ok(())
    }
}

#[test]
fn counter_detects_a_real_allocation() {
    let (_, allocations) = count(|| std::hint::black_box(vec![1_u8; 4096]));
    assert!(allocations > 0);
}

#[test]
fn unused_query_and_four_unescaped_pairs_do_not_allocate() {
    let (_, allocations) = count(|| {
        let unused = QueryParams::new(Some("q=expensive%20query&unused=%E4%B8%AD"));
        std::hint::black_box(unused);
        let params = QueryParams::new(Some("a=1&b=2&a=3&d=four"));
        assert_eq!(params.get("d"), Some("four"));
        assert_eq!(params.get("a"), Some("3"));
        assert_eq!(params.values("a").count(), 2);
    });
    assert_eq!(allocations, 0);
}

#[cfg(any(feature = "api-log", feature = "slow-log"))]
#[test]
fn metadata_borrows_after_parser_descriptors_are_dropped() {
    let head = b"GET /items?q=x HTTP/1.1\r\nx-request-id: req 123\r\nx-forwarded-for: 203.0.113.7, 198.51.100.4\r\n\r\n";
    let peer = "127.0.0.1:1".parse().unwrap();
    let (metadata, allocations) = count(|| {
        let mut headers = [httparse::EMPTY_HEADER; 8];
        let mut parsed = httparse::Request::new(&mut headers);
        assert!(parsed.parse(head).unwrap().is_complete());
        logging::RequestLog::new(head, &parsed, peer, 7, true)
    });
    assert_eq!(allocations, 0);
    assert_eq!(metadata.method(head).as_ptr(), head.as_ptr());
    assert_eq!(metadata.target(head).as_ptr(), head[4..].as_ptr());
    assert_eq!(metadata.peer, peer);
    assert_eq!(metadata.request_len, 7);
    let mut sink = Sink::default();
    let (_, allocations) = count(|| {
        write!(
            &mut sink,
            "{} {}",
            metadata.method(head),
            metadata.target(head)
        )
        .unwrap();
        #[cfg(feature = "api-log")]
        write!(
            &mut sink,
            " {} {}",
            metadata.client_ip(head),
            metadata.request_id(head)
        )
        .unwrap();
    });
    assert_eq!(allocations, 0);
    assert!(sink.0 > 0);
}

#[cfg(feature = "slow-log")]
#[test]
fn large_segmented_body_excerpt_does_not_allocate_or_consume_body() {
    let arena = brz_http_server::EphemeralBytesArena::new(32 * 1024);
    let mut writer = brz_io::Writer::new(&arena);
    let payload = "a€😀".repeat(64 * 1024);
    std::io::Write::write_all(&mut writer, payload.as_bytes()).unwrap();
    let body = writer.into_reader();
    let mut sink = Sink::default();
    let (_, allocations) = count(|| {
        write!(&mut sink, "{}", logging::BodyExcerpt(Some(body.view()))).unwrap();
    });
    assert_eq!(
        allocations, 0,
        "the excerpt must not trigger the contiguous body cache"
    );
    assert!(sink.0 <= 512);
    assert_eq!(body.len(), payload.len());
    assert_eq!(body.position(), 0);
}
