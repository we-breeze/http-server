use std::mem::size_of;
use std::time::{Duration, Instant};

#[cfg(feature = "api-log")]
use std::fmt::Write as _;
#[cfg(feature = "api-log")]
use std::sync::OnceLock;

#[cfg(feature = "api-log")]
use brz_ds::SmolStr;
#[cfg(feature = "metrics")]
use brz_metrics::Metric;

use crate::StatusCode;

/// Fixed metric handles generated for an exported route template.
#[derive(Clone, Copy, Debug)]
pub struct ApiMetrics {
    #[cfg(feature = "metrics")]
    statuses: [Metric; 4],
    #[cfg(feature = "metrics")]
    timeout: Metric,
}

impl ApiMetrics {
    /// Register status-class and timeout metrics under one route template.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        #[cfg(not(feature = "metrics"))]
        let _ = name;
        Self {
            #[cfg(feature = "metrics")]
            statuses: [
                Metric::api(name),
                Metric::api_3xx(name),
                Metric::api_4xx(name),
                Metric::api_5xx(name),
            ],
            #[cfg(feature = "metrics")]
            timeout: Metric::api_timeout(name),
        }
    }

    #[cfg(feature = "metrics")]
    fn record(self, status: StatusCode, elapsed: Duration) {
        let class = status.as_u16() / 100;
        if (2..=5).contains(&class) {
            self.statuses[usize::from(class - 2)].record(elapsed, class < 4);
        }
    }

    #[cfg(not(feature = "metrics"))]
    fn record(self, _status: StatusCode, _elapsed: Duration) {
        let _ = self;
    }

    #[cfg(feature = "metrics")]
    fn record_timeout(self, elapsed: Duration) {
        self.timeout.record(elapsed, false);
    }

    #[cfg(not(feature = "metrics"))]
    fn record_timeout(self, _elapsed: Duration) {
        let _ = self;
    }
}

/// Longest method token kept in a log line. A real method is shorter.
#[cfg(any(feature = "api-log", feature = "slow-log"))]
const METHOD_BYTES: usize = 16;

/// Longest request target kept in a log line.
#[cfg(any(feature = "api-log", feature = "slow-log"))]
const TARGET_BYTES: usize = 128;

/// Longest request-body excerpt kept in a slow line.
#[cfg(feature = "slow-log")]
const EXCERPT_BYTES: usize = 512;

/// A fixed-capacity log field that keeps a prefix and drops the rest.
///
/// The method token, the request target, and the body excerpt are bounded by
/// construction, so a value past the capacity is malformed input already and
/// dropping its tail costs less than a heap allocation on every request. Having
/// no heap arm and no discriminant also makes this the smallest representation
/// for those three fields.
#[cfg(any(feature = "api-log", feature = "slow-log"))]
struct LogField<const N: usize> {
    bytes: [u8; N],
    len: u16,
}

#[cfg(any(feature = "api-log", feature = "slow-log"))]
impl<const N: usize> LogField<N> {
    const fn new() -> Self {
        const { assert!(N <= u16::MAX as usize, "field capacity must fit the length") };
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    /// Borrows the text. Only whole `&str` encodings are ever copied in, so
    /// the UTF-8 invariant needs no unsafe to hold.
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("field holds UTF-8")
    }

    /// Appends as much of `value` as fits and drops the rest.
    ///
    /// A character is never split: an over-long write stops at the last
    /// boundary that fits, so the field always holds valid UTF-8.
    fn push_str(&mut self, value: &str) {
        let start = usize::from(self.len);
        let room = N - start;
        if value.len() <= room {
            let end = start + value.len();
            self.bytes[start..end].copy_from_slice(value.as_bytes());
            self.len = end as u16;
            return;
        }
        let mut end = room;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes[start..start + end].copy_from_slice(&value.as_bytes()[..end]);
        self.len = (start + end) as u16;
    }
}

#[cfg(any(feature = "api-log", feature = "slow-log"))]
impl<const N: usize> Default for LogField<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(feature = "api-log", feature = "slow-log"))]
impl<const N: usize> From<&str> for LogField<N> {
    fn from(value: &str) -> Self {
        let mut field = Self::new();
        field.push_str(value);
        field
    }
}

#[cfg(any(feature = "api-log", feature = "slow-log"))]
impl<const N: usize> std::fmt::Display for LogField<N> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(any(feature = "api-log", feature = "slow-log"))]
struct RequestLog {
    method: LogField<METHOD_BYTES>,
    target: LogField<TARGET_BYTES>,
    #[cfg(feature = "api-log")]
    request_id: SmolStr,
    #[cfg(feature = "api-log")]
    client_ip: SmolStr,
    #[cfg(feature = "slow-log")]
    peer: std::net::SocketAddr,
    request_len: usize,
    #[cfg(feature = "slow-log")]
    body: LogField<EXCERPT_BYTES>,
}

#[cfg(feature = "api-log")]
#[derive(Debug, Default)]
pub(crate) struct ApiLogContext {
    enabled: bool,
    auth_id: OnceLock<SmolStr>,
}

#[cfg(feature = "api-log")]
impl ApiLogContext {
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Formats the principal identifier straight into its field, so a short
    /// identifier is recorded without an intermediate allocation and a long
    /// one spills instead of being cut.
    pub(crate) fn set_auth_id(&self, id: &dyn std::fmt::Display) {
        if !self.enabled {
            return;
        }
        let mut field = SmolStr::new();
        write!(field, "{id}").expect("writing a field into a SmolStr cannot fail");
        let _ = self.auth_id.set(field);
    }

    fn auth_id(&self) -> &str {
        self.auth_id.get().map_or("-", SmolStr::as_str)
    }
}

pub(crate) struct Observation {
    metrics: Option<ApiMetrics>,
    api_log: bool,
    started: Instant,
    #[cfg(any(feature = "api-log", feature = "slow-log"))]
    request: Option<RequestLog>,
    #[cfg(feature = "api-log")]
    api_log_context: ApiLogContext,
}

/// One `Observation` lives on the stack for the whole of every connection, so
/// its footprint is bounded deliberately: the log fields hold their values
/// inline and no field grows without limit. Raising a field capacity is a
/// decision about that footprint, and this keeps it a visible one.
///
/// The shape narrows with the features, so the bound is stated per shape.
#[cfg(feature = "slow-log")]
const _: () = assert!(
    size_of::<Observation>() <= 1024,
    "Observation exceeded its 1024-byte budget; check the field capacities"
);

#[cfg(not(feature = "slow-log"))]
const _: () = assert!(
    size_of::<Observation>() <= 512,
    "Observation exceeded its 512-byte budget without slow-log"
);

impl Observation {
    pub(crate) fn new() -> Self {
        Self {
            metrics: None,
            api_log: true,
            started: Instant::now(),
            #[cfg(any(feature = "api-log", feature = "slow-log"))]
            request: None,
            #[cfg(feature = "api-log")]
            api_log_context: ApiLogContext::default(),
        }
    }

    pub(crate) fn matched(&mut self, metrics: Option<ApiMetrics>) {
        self.metrics = metrics;
        self.started = Instant::now();
    }

    pub(crate) fn set_api_log(&mut self, enabled: bool) {
        self.api_log = enabled;
        #[cfg(feature = "api-log")]
        self.api_log_context.set_enabled(enabled);
    }

    pub(crate) fn request_head(
        &mut self,
        method: &str,
        target: &str,
        peer: std::net::SocketAddr,
        request_len: usize,
        ids: (Option<&[u8]>, Option<&[u8]>),
    ) {
        #[cfg(any(feature = "api-log", feature = "slow-log"))]
        {
            #[cfg(feature = "api-log")]
            let (request_id, forwarded_for) = ids;
            #[cfg(not(feature = "api-log"))]
            let _ = ids;
            self.request = Some(RequestLog {
                method: method.into(),
                target: target.into(),
                #[cfg(feature = "api-log")]
                request_id: request_id_value(request_id),
                #[cfg(feature = "api-log")]
                client_ip: client_ip_value(forwarded_for, peer.ip()),
                #[cfg(feature = "slow-log")]
                peer,
                request_len,
                #[cfg(feature = "slow-log")]
                body: LogField::new(),
            });
        }
        #[cfg(not(any(feature = "api-log", feature = "slow-log")))]
        let _ = (self, method, target, peer, request_len, ids);
    }

    #[cfg(feature = "slow-log")]
    pub(crate) fn request_body(&mut self, body: &[u8]) {
        if let Some(request) = &mut self.request {
            request.body = excerpt(body);
        }
    }

    #[cfg(feature = "api-log")]
    pub(crate) fn api_log_context(&self) -> &ApiLogContext {
        &self.api_log_context
    }

    pub(crate) fn record(&self, status: StatusCode, response_len: Option<u64>, timed_out: bool) {
        #[cfg(not(any(feature = "api-log", feature = "slow-log")))]
        let _ = response_len;
        let elapsed = self.started.elapsed();
        if let Some(metrics) = self.metrics {
            if timed_out {
                metrics.record_timeout(elapsed);
            } else {
                metrics.record(status, elapsed);
            }
        }
        #[cfg(feature = "api-log")]
        if self.api_log
            && let Some(request) = &self.request
        {
            tracing::info!(
                target: "breeze.api",
                "{} {} {} {}ms {} {} {} {} {}",
                request.method,
                request.target,
                status.as_u16(),
                elapsed.as_millis(),
                request.request_len,
                OptionalLength(response_len),
                request.client_ip,
                self.api_log_context.auth_id(),
                request.request_id,
            );
        }
        #[cfg(feature = "slow-log")]
        if elapsed >= Duration::from_secs(3)
            && let Some(request) = &self.request
        {
            tracing::warn!(
                target: "breeze.slow",
                "http-server {} {} {} {}ms {} {} {} {} {}",
                request.method,
                request.target,
                status.as_u16(),
                elapsed.as_millis(),
                request.request_len,
                OptionalLength(response_len),
                request.peer,
                timed_out,
                request.body,
            );
        }
    }
}

/// Written for a request ID that is present but not UTF-8.
#[cfg(feature = "api-log")]
const INVALID_REQUEST_ID: &str = "<invalid-request-id>";

/// Written for a forwarded chain that is present but not UTF-8.
#[cfg(feature = "api-log")]
const INVALID_CLIENT_IP: &str = "<invalid-client-ip>";

/// Reads a header value into a log field, dropping ASCII whitespace because the
/// line is positional, and reports whether the bytes were UTF-8.
#[cfg(feature = "api-log")]
fn push_stripped(field: &mut SmolStr, bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    for character in text.chars() {
        if !character.is_ascii_whitespace() {
            field.push_char(character);
        }
    }
    true
}

/// Records the `x-request-id` value whole. A value that is not UTF-8 is
/// reported as such, and an absent, empty, or whitespace-only one keeps the
/// placeholder that holds the positional line together.
#[cfg(feature = "api-log")]
fn request_id_value(value: Option<&[u8]>) -> SmolStr {
    let mut field = SmolStr::new();
    match value {
        None | Some([]) => {}
        Some(bytes) => {
            if !push_stripped(&mut field, bytes) {
                field.push_str(INVALID_REQUEST_ID);
            }
        }
    }
    if field.is_empty() {
        field.push_char('-');
    }
    field
}

/// Records the whole `x-forwarded-for` chain the gateway appends to, so triage
/// can read every hop rather than one of them, and falls back to the accepted
/// peer when the header holds no address. Both are kept whole: a field is
/// never truncated, so a caller that needs a bound bounds it before storing.
#[cfg(feature = "api-log")]
fn client_ip_value(value: Option<&[u8]>, peer: std::net::IpAddr) -> SmolStr {
    if let Some(bytes) = value.filter(|bytes| !bytes.is_empty()) {
        let mut field = SmolStr::new();
        if !push_stripped(&mut field, bytes) {
            field.push_str(INVALID_CLIENT_IP);
            return field;
        }
        // An empty or separator-only chain carries no address.
        if field.as_str().bytes().any(|byte| byte != b',') {
            return field;
        }
    }
    let mut field = SmolStr::new();
    write!(field, "{peer}").expect("writing an address into a SmolStr cannot fail");
    field
}

/// Copies a body excerpt into a field, dropping what does not fit.
///
/// A body that is already UTF-8 is copied in one pass. Anything else goes
/// through a lossy conversion first, which allocates only for a body that is
/// not UTF-8 at all.
#[cfg(feature = "slow-log")]
fn excerpt(bytes: &[u8]) -> LogField<EXCERPT_BYTES> {
    let source = &bytes[..bytes.len().min(EXCERPT_BYTES)];
    let mut field = LogField::new();
    match std::str::from_utf8(source) {
        Ok(text) => field.push_str(text),
        Err(_) => field.push_str(&String::from_utf8_lossy(source)),
    }
    field
}

#[cfg(any(feature = "api-log", feature = "slow-log"))]
struct OptionalLength(Option<u64>);

#[cfg(any(feature = "api-log", feature = "slow-log"))]
impl std::fmt::Display for OptionalLength {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(length) => length.fmt(formatter),
            None => formatter.write_str("-"),
        }
    }
}

#[cfg(all(test, feature = "api-log"))]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span;
    use tracing::{Event, Metadata, Subscriber};

    use super::{
        ApiLogContext, LogField, Observation, TARGET_BYTES, client_ip_value, request_id_value,
    };
    #[cfg(feature = "slow-log")]
    use super::{EXCERPT_BYTES, excerpt};
    use crate::StatusCode;

    fn peer() -> std::net::IpAddr {
        "203.0.113.7".parse().expect("valid peer")
    }

    /// Captures the message of every event emitted on this thread.
    #[derive(Default)]
    struct Captured(Arc<Mutex<Vec<String>>>);

    impl Subscriber for Captured {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _attributes: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            struct Message(String);
            impl Visit for Message {
                fn record_str(&mut self, field: &Field, value: &str) {
                    if field.name() == "message" {
                        value.clone_into(&mut self.0);
                    }
                }

                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut message = Message(String::new());
            event.record(&mut message);
            self.0.lock().expect("log capture lock").push(message.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }

    #[test]
    fn request_id_uses_placeholders_for_missing_empty_and_non_utf8_values() {
        assert_eq!(request_id_value(None).as_str(), "-");
        assert_eq!(request_id_value(Some(b"")).as_str(), "-");
        assert_eq!(request_id_value(Some(b"   ")).as_str(), "-");
        assert_eq!(
            request_id_value(Some(b"request-123")).as_str(),
            "request-123"
        );
        // Not UTF-8 at all: reported rather than dropped.
        assert_eq!(
            request_id_value(Some(b"\xff\xfe")).as_str(),
            "<invalid-request-id>"
        );
    }

    #[test]
    fn log_fields_never_contain_whitespace() {
        assert_eq!(request_id_value(Some(b"req 123")).as_str(), "req123");
        assert_eq!(
            client_ip_value(Some(b"198.51.100.4,\t203.0.113.9"), peer()).as_str(),
            "198.51.100.4,203.0.113.9"
        );
    }

    #[test]
    fn a_request_id_is_kept_whole_including_non_ascii_text() {
        let long = "a".repeat(200);
        assert_eq!(request_id_value(Some(long.as_bytes())).as_str(), long);
        assert_eq!(
            request_id_value(Some("请求-1".as_bytes())).as_str(),
            "请求-1"
        );
    }

    #[test]
    fn client_ip_falls_back_to_the_peer_without_a_usable_chain() {
        assert_eq!(client_ip_value(None, peer()).as_str(), "203.0.113.7");
        assert_eq!(client_ip_value(Some(b""), peer()).as_str(), "203.0.113.7");
        assert_eq!(
            client_ip_value(Some(b" , "), peer()).as_str(),
            "203.0.113.7"
        );
        assert_eq!(
            client_ip_value(Some(b"\xff"), peer()).as_str(),
            "<invalid-client-ip>"
        );
    }

    #[test]
    fn client_ip_records_the_whole_chain_including_every_hop() {
        let chain = format!("{}203.0.113.9", "1.1.1.1,".repeat(30));
        let field = client_ip_value(Some(chain.as_bytes()), peer());
        assert_eq!(field.as_str(), chain, "no hop is dropped");
        assert_eq!(
            client_ip_value(Some(b"198.51.100.4, 203.0.113.9"), peer()).as_str(),
            "198.51.100.4,203.0.113.9"
        );
    }

    #[test]
    fn the_api_line_places_the_client_address_before_the_principal() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut observation = Observation::new();
        observation.matched(None);
        tracing::subscriber::with_default(Captured(Arc::clone(&captured)), || {
            observation.request_head(
                "GET",
                "/api/items?q=a",
                "127.0.0.1:1".parse().expect("valid peer"),
                128,
                (Some(b"req-123"), Some(b"203.0.113.7, 198.51.100.4")),
            );
            observation.record(StatusCode::OK, Some(512), false);
        });
        let lines = captured.lock().expect("log capture lock");
        let line = lines.first().expect("one api line");
        assert!(line.starts_with("GET /api/items?q=a 200 "), "{line}");
        assert!(
            line.contains("128 512 203.0.113.7,198.51.100.4 - req-123"),
            "{line}"
        );
    }

    #[test]
    fn a_target_past_the_field_capacity_is_cut_and_keeps_the_prefix() {
        let target = format!("/api/items?q={}", "x".repeat(200));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut observation = Observation::new();
        observation.matched(None);
        tracing::subscriber::with_default(Captured(Arc::clone(&captured)), || {
            observation.request_head(
                "GET",
                &target,
                "127.0.0.1:1".parse().expect("valid peer"),
                0,
                (None, None),
            );
            observation.record(StatusCode::OK, Some(2), false);
        });
        let lines = captured.lock().expect("log capture lock");
        let line = lines.first().expect("one api line");
        assert!(line.contains(&target[..TARGET_BYTES]), "{line}");
        assert!(!line.contains(&target), "the tail is dropped: {line}");
    }

    #[test]
    fn a_field_keeps_a_prefix_and_never_splits_a_character() {
        let mut field: LogField<8> = LogField::from("abc");
        assert_eq!(field.as_str(), "abc");

        field.push_str("defghij");
        assert_eq!(field.as_str(), "abcdefgh", "the tail is dropped");

        // Two bytes of room and a three-byte character: none of it is written.
        let mut field: LogField<4> = LogField::from("ab");
        field.push_str("€");
        assert_eq!(field.as_str(), "ab");

        // One euro fits, the next does not, and no half character is left.
        let mut field: LogField<5> = LogField::from("ab");
        field.push_str("€€");
        assert_eq!(field.as_str(), "ab€");
        assert_eq!(field.as_str().chars().count(), 3);
    }

    #[cfg(feature = "slow-log")]
    #[test]
    fn a_body_excerpt_is_bounded_and_stays_valid_utf8() {
        let long = "x".repeat(EXCERPT_BYTES + 100);
        assert_eq!(excerpt(long.as_bytes()).as_str().len(), EXCERPT_BYTES);

        let body = b"ok\xff\xfe";
        assert_eq!(excerpt(body).as_str(), "ok\u{fffd}\u{fffd}");
    }

    #[test]
    fn auth_id_is_written_only_for_enabled_api_logs() {
        let disabled = ApiLogContext::default();
        disabled.set_auth_id(&"ignored");
        assert_eq!(disabled.auth_id(), "-");

        let mut enabled = ApiLogContext::default();
        enabled.set_enabled(true);
        enabled.set_auth_id(&"alice");
        assert_eq!(enabled.auth_id(), "alice");
        assert!(
            !enabled
                .auth_id
                .get()
                .expect("the identifier was set")
                .is_heap_allocated(),
            "a short identifier stays inline"
        );
    }

    #[test]
    fn a_long_principal_identifier_spills_instead_of_being_cut() {
        let mut context = ApiLogContext::default();
        context.set_enabled(true);
        let identifier = "p".repeat(200);
        context.set_auth_id(&identifier);
        assert_eq!(context.auth_id(), identifier);
        assert!(
            context
                .auth_id
                .get()
                .expect("the identifier was set")
                .is_heap_allocated()
        );
    }
}
