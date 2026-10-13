use std::mem::size_of;
use std::time::{Duration, Instant};

#[cfg(feature = "api-log")]
use std::fmt::Write as _;
#[cfg(any(feature = "api-log", feature = "metrics"))]
use std::sync::OnceLock;

#[cfg(feature = "api-log")]
use brz_ds::SmolStr;
#[cfg(feature = "metrics")]
use brz_metrics::{CounterMetric, MetricType, TimingMetric};
#[cfg(feature = "metrics")]
use std::sync::Arc;

use crate::StatusCode;

/// Cumulative route metrics with a lazily populated exact-status cache.
#[derive(Clone, Debug)]
pub struct ApiMetrics {
    #[cfg(feature = "metrics")]
    requests: TimingMetric,
    #[cfg(feature = "metrics")]
    responses: Arc<ResponseMetrics>,
    #[cfg(feature = "metrics")]
    timeout: CounterMetric,
}

// A sparse two-level cache keeps the request path lock-free after first use,
// without allocating 500 full OnceLock<CounterMetric> entries for every route.
#[cfg(feature = "metrics")]
#[derive(Debug)]
struct ResponseMetrics {
    name: &'static str,
    groups: [OnceLock<Box<[OnceLock<CounterMetric>; 10]>>; 50],
}

#[cfg(feature = "metrics")]
impl ResponseMetrics {
    fn record(&self, status: u16) {
        // Status sanitization happens before Observation::record, but retain a
        // bounded guard for callers and future changes to response normalization.
        if !(100..=599).contains(&status) {
            return;
        }
        let index = usize::from(status - 100);
        let group = self.groups[index / 10]
            .get_or_init(|| Box::new(std::array::from_fn(|_| OnceLock::new())));
        let metric = group[index % 10]
            .get_or_init(|| CounterMetric::new(MetricType::Api(status), self.name));
        metric.inc();
    }
}

impl ApiMetrics {
    /// Register all requests under one route template.
    ///
    /// Responses below 400 are successful; 4xx, 5xx, and timeouts are errors.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        #[cfg(not(feature = "metrics"))]
        let _ = name;
        Self {
            #[cfg(feature = "metrics")]
            requests: TimingMetric::new(MetricType::ApiAll, name),
            #[cfg(feature = "metrics")]
            responses: Arc::new(ResponseMetrics {
                name,
                groups: std::array::from_fn(|_| OnceLock::new()),
            }),
            #[cfg(feature = "metrics")]
            timeout: CounterMetric::new(MetricType::ApiTimeout, name),
        }
    }

    #[cfg(feature = "metrics")]
    fn record(&self, status: StatusCode, elapsed: Duration) {
        self.requests.record(elapsed, status.as_u16() < 400);
        self.responses.record(status.as_u16());
    }

    #[cfg(not(feature = "metrics"))]
    fn record(&self, _status: StatusCode, _elapsed: Duration) {}

    #[cfg(feature = "metrics")]
    fn record_timeout(&self, elapsed: Duration) {
        self.requests.record(elapsed, false);
        self.timeout.inc();
    }

    #[cfg(not(feature = "metrics"))]
    fn record_timeout(&self, _elapsed: Duration) {}
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
        if let Some(metrics) = &self.metrics {
            if timed_out {
                metrics.record_timeout(elapsed);
                #[cfg(feature = "metrics")]
                metrics.responses.record(status.as_u16());
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
