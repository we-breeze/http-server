use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{Event, Metadata, Subscriber, span};

#[cfg(feature = "api-log")]
use super::ApiLogContext;
use super::Observation;
use crate::StatusCode;

#[derive(Default)]
struct Captured(Arc<Mutex<Vec<String>>>);

impl Subscriber for Captured {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        struct Message(String);
        impl Visit for Message {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "message" {
                    value.clone_into(&mut self.0);
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0.lock().unwrap().push(message.0);
    }
}

fn prepare(observation: &mut Observation, head: &[u8], request_len: usize) {
    let mut headers = [httparse::EMPTY_HEADER; 8];
    let mut parsed = httparse::Request::new(&mut headers);
    assert!(parsed.parse(head).unwrap().is_complete());
    observation.request_head(head, &parsed, "127.0.0.1:1".parse().unwrap(), request_len);
    // Neither `parsed` nor its descriptor array survives this function.
}

#[cfg(feature = "api-log")]
#[test]
fn api_line_preserves_field_order_after_parser_is_dropped() {
    let head = b"GET /api/items?q=a HTTP/1.1\r\nx-request-id: req-123\r\nx-forwarded-for: 203.0.113.7, 198.51.100.4\r\n\r\n";
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut observation = Observation::new();
    observation.matched(None);
    observation.set_api_log(true);
    prepare(&mut observation, head, 128);
    tracing::subscriber::with_default(Captured(Arc::clone(&captured)), || {
        observation.record(StatusCode::OK, Some(512), false, head, None);
    });
    let lines = captured.lock().unwrap();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert!(line.starts_with("GET /api/items?q=a 200 "), "{line}");
    assert!(
        line.contains("128 512 203.0.113.7,198.51.100.4 - req-123"),
        "{line}"
    );
}

#[cfg(feature = "api-log")]
#[test]
fn overlong_target_is_truncated_without_a_separate_string() {
    let target = format!("/api/items?q={}", "x".repeat(200));
    let head = format!("GET {target} HTTP/1.1\r\n\r\n");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut observation = Observation::new();
    prepare(&mut observation, head.as_bytes(), 0);
    tracing::subscriber::with_default(Captured(Arc::clone(&captured)), || {
        observation.record(StatusCode::OK, Some(2), false, head.as_bytes(), None);
    });
    let lines = captured.lock().unwrap();
    assert!(lines[0].contains(&target[..128]));
    assert!(!lines[0].contains(&target));
}

#[test]
fn disabled_api_log_still_allows_slow_log_but_never_emits_api_line() {
    let head = b"POST /private HTTP/1.1\r\nx-request-id: ignored\r\n\r\n";
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut observation = Observation::new();
    observation.set_api_log(false);
    prepare(&mut observation, head, 0);
    observation.started = Instant::now() - Duration::from_secs(4);
    tracing::subscriber::with_default(Captured(Arc::clone(&captured)), || {
        observation.record(StatusCode::OK, Some(0), false, head, None);
    });
    let lines = captured.lock().unwrap();
    #[cfg(feature = "slow-log")]
    {
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("http-server POST /private 200 "));
    }
    #[cfg(not(feature = "slow-log"))]
    {
        assert!(lines.is_empty());
        assert!(
            observation.request.is_none(),
            "disabled logs collect no metadata"
        );
    }
}

#[cfg(feature = "slow-log")]
#[test]
fn slow_line_uses_borrowed_body_and_preserves_timeout_column() {
    use std::io::Write;
    let head = b"POST /items HTTP/1.1\r\n\r\n";
    let arena = crate::EphemeralBytesArena::new(2);
    let mut writer = brz_io::Writer::new(&arena);
    writer.write_all("ok€😀".as_bytes()).unwrap();
    let body = writer.into_reader();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut observation = Observation::new();
    observation.set_api_log(false);
    prepare(&mut observation, head, body.len());
    observation.started = Instant::now() - Duration::from_secs(4);
    tracing::subscriber::with_default(Captured(Arc::clone(&captured)), || {
        observation.record(
            StatusCode::REQUEST_TIMEOUT,
            Some(0),
            true,
            head,
            Some(body.view()),
        );
    });
    let lines = captured.lock().unwrap();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("http-server POST /items 408 "));
    assert!(lines[0].ends_with("127.0.0.1:1 true ok€😀"), "{}", lines[0]);
    assert_eq!(body.position(), 0);
}

#[cfg(feature = "slow-log")]
#[test]
fn a_fast_request_with_only_slow_log_emits_nothing() {
    let head = b"GET /fast HTTP/1.1\r\n\r\n";
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut observation = Observation::new();
    observation.set_api_log(false);
    prepare(&mut observation, head, 0);
    tracing::subscriber::with_default(Captured(Arc::clone(&captured)), || {
        observation.record(StatusCode::OK, Some(0), false, head, None);
    });
    assert!(captured.lock().unwrap().is_empty());
}

#[cfg(feature = "api-log")]
#[test]
fn principal_keeps_inline_and_spill_behavior_and_formats_only_once() {
    let disabled = ApiLogContext::default();
    disabled.set_auth_id(&"ignored");
    assert_eq!(disabled.auth_id(), "-");
    let mut enabled = ApiLogContext::default();
    enabled.set_enabled(true);
    enabled.set_auth_id(&"alice");
    assert_eq!(enabled.auth_id(), "alice");
    assert!(!enabled.auth_id.get().unwrap().is_heap_allocated());
    struct MustNotFormat;
    impl std::fmt::Display for MustNotFormat {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("a later principal must not be formatted");
        }
    }
    enabled.set_auth_id(&MustNotFormat);
    assert_eq!(enabled.auth_id(), "alice");
    let long = "p".repeat(200);
    let mut spilled = ApiLogContext::default();
    spilled.set_enabled(true);
    spilled.set_auth_id(&long);
    assert_eq!(spilled.auth_id(), long);
    assert!(spilled.auth_id.get().unwrap().is_heap_allocated());
}
