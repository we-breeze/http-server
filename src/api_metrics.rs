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

#[cfg(any(feature = "api-log", feature = "slow-log"))]
mod logging;
#[cfg(any(feature = "api-log", feature = "slow-log"))]
use logging::RequestLog;

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
        if !self.enabled || self.auth_id.get().is_some() {
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

/// Metadata stays inline; request strings and body bytes remain in caller-owned
/// storage through recording, including after cancellation of the handler.
/// Derived authentication identifiers retain the existing SmolStr spill policy.
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
        head: &[u8],
        parsed: &httparse::Request<'_, '_>,
        peer: std::net::SocketAddr,
        request_len: usize,
    ) {
        #[cfg(any(feature = "api-log", feature = "slow-log"))]
        {
            if !self.api_log && !cfg!(feature = "slow-log") {
                return;
            }
            self.request = Some(RequestLog::new(
                head,
                parsed,
                peer,
                request_len,
                self.api_log,
            ));
        }
        #[cfg(not(any(feature = "api-log", feature = "slow-log")))]
        let _ = (self, head, parsed, peer, request_len);
    }

    #[cfg(feature = "api-log")]
    pub(crate) fn api_log_context(&self) -> &ApiLogContext {
        &self.api_log_context
    }

    pub(crate) fn record(
        &self,
        status: StatusCode,
        response_len: Option<u64>,
        timed_out: bool,
        head: &[u8],
        body: Option<brz_io::ReaderView<'_>>,
    ) {
        #[cfg(not(any(feature = "api-log", feature = "slow-log")))]
        let _ = (response_len, head);
        #[cfg(not(feature = "slow-log"))]
        let _ = body;
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
                request.method(head),
                request.target(head),
                status.as_u16(),
                elapsed.as_millis(),
                request.request_len,
                OptionalLength(response_len),
                request.client_ip(head),
                self.api_log_context.auth_id(),
                request.request_id(head),
            );
        }
        #[cfg(feature = "slow-log")]
        if elapsed >= Duration::from_secs(3)
            && let Some(request) = &self.request
        {
            tracing::warn!(
                target: "breeze.slow",
                "http-server {} {} {} {}ms {} {} {} {} {}",
                request.method(head),
                request.target(head),
                status.as_u16(),
                elapsed.as_millis(),
                request.request_len,
                OptionalLength(response_len),
                request.peer,
                timed_out,
                logging::BodyExcerpt(body),
            );
        }
    }
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

#[cfg(all(test, any(feature = "api-log", feature = "slow-log")))]
mod tests;
