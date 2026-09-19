use std::time::{Duration, Instant};

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
    #[cfg(feature = "slow-log")]
    peer: std::net::SocketAddr,
    request_len: usize,
    #[cfg(feature = "slow-log")]
    body: Box<str>,
}

pub(crate) struct Observation {
    metrics: Option<ApiMetrics>,
    api_log: bool,
    started: Instant,
    #[cfg(any(feature = "api-log", feature = "slow-log"))]
    request: Option<RequestLog>,
}

impl Observation {
    pub(crate) fn new() -> Self {
        Self {
            metrics: None,
            api_log: true,
            started: Instant::now(),
            #[cfg(any(feature = "api-log", feature = "slow-log"))]
            request: None,
        }
    }

    pub(crate) fn matched(&mut self, metrics: Option<ApiMetrics>) {
        self.metrics = metrics;
        self.started = Instant::now();
    }

    pub(crate) fn set_api_log(&mut self, enabled: bool) {
        self.api_log = enabled;
    }

    pub(crate) fn request_head(
        &mut self,
        method: &str,
        target: &str,
        peer: std::net::SocketAddr,
        request_len: usize,
    ) {
        #[cfg(any(feature = "api-log", feature = "slow-log"))]
        {
            #[cfg(not(feature = "slow-log"))]
            let _ = peer;
            self.request = Some(RequestLog {
                method: method.into(),
                target: target.into(),
                #[cfg(feature = "slow-log")]
                peer,
                request_len,
                #[cfg(feature = "slow-log")]
                body: Box::default(),
            });
        }
        #[cfg(not(any(feature = "api-log", feature = "slow-log")))]
        let _ = (self, method, target, peer, request_len);
    }

    pub(crate) fn request(&mut self, request: &crate::Request<'_>) {
        #[cfg(any(feature = "api-log", feature = "slow-log"))]
        {
            let body = request.body();
            self.request = Some(RequestLog {
                method: request.method().into(),
                target: request.target().into(),
                #[cfg(feature = "slow-log")]
                peer: request.peer_addr(),
                request_len: body.len(),
                #[cfg(feature = "slow-log")]
                body: truncate_detail(body),
            });
        }
        #[cfg(not(any(feature = "api-log", feature = "slow-log")))]
        let _ = (self, request);
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
                "{} {} {} {}ms {} {}",
                request.method,
                request.target,
                status.as_u16(),
                elapsed.as_millis(),
                request.request_len,
                OptionalLength(response_len),
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
