use std::time::{Duration, Instant};

#[cfg(feature = "api-log")]
use std::sync::OnceLock;

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

#[cfg(any(feature = "api-log", feature = "slow-log"))]
struct RequestLog {
    method: Box<str>,
    target: Box<str>,
    #[cfg(feature = "api-log")]
    request_id: RequestId,
    #[cfg(feature = "slow-log")]
    peer: std::net::SocketAddr,
    request_len: usize,
    #[cfg(feature = "slow-log")]
    body: Box<str>,
}

#[cfg(feature = "api-log")]
#[derive(Debug, Default)]
pub(crate) struct ApiLogContext {
    enabled: bool,
    auth_id: OnceLock<Box<str>>,
}

#[cfg(feature = "api-log")]
impl ApiLogContext {
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn set_auth_id(&self, id: &dyn std::fmt::Display) {
        if !self.enabled {
            return;
        }
        let _ = self.auth_id.set(id.to_string().into_boxed_str());
    }

    fn auth_id(&self) -> &str {
        self.auth_id.get().map_or("-", AsRef::as_ref)
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
        request_id: Option<&[u8]>,
    ) {
        #[cfg(any(feature = "api-log", feature = "slow-log"))]
        {
            #[cfg(not(feature = "slow-log"))]
            let _ = peer;
            self.request = Some(RequestLog {
                method: method.into(),
                target: target.into(),
                #[cfg(feature = "api-log")]
                request_id: request_id_value(request_id),
                #[cfg(feature = "slow-log")]
                peer,
                request_len,
                #[cfg(feature = "slow-log")]
                body: Box::default(),
            });
        }
        #[cfg(not(any(feature = "api-log", feature = "slow-log")))]
        let _ = (self, method, target, peer, request_len, request_id);
        #[cfg(all(feature = "slow-log", not(feature = "api-log")))]
        let _ = request_id;
    }

    #[cfg(feature = "slow-log")]
    pub(crate) fn request_body(&mut self, body: &[u8]) {
        if let Some(request) = &mut self.request {
            request.body = truncate_detail(body);
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
                "{} {} {} {}ms {} {} {} {}",
                request.method,
                request.target,
                status.as_u16(),
                elapsed.as_millis(),
                request.request_len,
                OptionalLength(response_len),
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

#[cfg(feature = "api-log")]
enum RequestId {
    Missing,
    Invalid,
    Value(Box<str>),
}

#[cfg(feature = "api-log")]
impl std::fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("-"),
            Self::Invalid => formatter.write_str("<invalid-request-id>"),
            Self::Value(value) => value.fmt(formatter),
        }
    }
}

#[cfg(feature = "api-log")]
fn request_id_value(value: Option<&[u8]>) -> RequestId {
    match value {
        None | Some([]) => RequestId::Missing,
        Some(value) if value.is_ascii() => RequestId::Value(
            std::str::from_utf8(value)
                .expect("ASCII request ID is valid UTF-8")
                .into(),
        ),
        Some(_) => RequestId::Invalid,
    }
}

#[cfg(feature = "slow-log")]
fn truncate_detail(bytes: &[u8]) -> Box<str> {
    const MAX_DETAIL_BYTES: usize = 2 * 1024;
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_DETAIL_BYTES)]).into()
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
    use super::{ApiLogContext, request_id_value};

    #[test]
    fn request_id_uses_placeholders_for_missing_empty_and_non_ascii_values() {
        assert_eq!(request_id_value(None).to_string(), "-");
        assert_eq!(request_id_value(Some(b"")).to_string(), "-");
        assert_eq!(
            request_id_value(Some(b"request-123")).to_string(),
            "request-123"
        );
        assert_eq!(
            request_id_value(Some("请求".as_bytes())).to_string(),
            "<invalid-request-id>"
        );
    }

    #[test]
    fn auth_id_is_formatted_only_for_enabled_api_logs() {
        let disabled = ApiLogContext::default();
        disabled.set_auth_id(&"ignored");
        assert_eq!(disabled.auth_id(), "-");

        let mut enabled = ApiLogContext::default();
        enabled.set_enabled(true);
        enabled.set_auth_id(&"alice");
        assert_eq!(enabled.auth_id(), "alice");
    }
}
