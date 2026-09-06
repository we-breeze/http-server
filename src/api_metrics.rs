use std::time::{Duration, Instant};

use brz_metrics::Metric;

use crate::StatusCode;

/// Fixed metric handles generated for an exported route template.
#[derive(Clone, Copy, Debug)]
pub struct ApiMetrics([Metric; 4]);

impl ApiMetrics {
    /// Register all four status classes. Repeated registrations share slots.
    #[must_use]
    pub fn new(names: [&'static str; 4]) -> Self {
        Self(names.map(Metric::api))
    }

    fn record(self, status: StatusCode, elapsed: Duration) {
        let class = status.as_u16() / 100;
        if (2..=5).contains(&class) {
            self.0[usize::from(class - 2)].record(elapsed, class < 4);
        }
    }
}

pub(crate) struct Observation {
    metrics: Option<ApiMetrics>,
    started: Instant,
}

impl Observation {
    pub(crate) fn new() -> Self {
        Self {
            metrics: None,
            started: Instant::now(),
        }
    }

    pub(crate) fn matched(&mut self, metrics: Option<ApiMetrics>) {
        self.metrics = metrics;
        self.started = Instant::now();
    }

    pub(crate) fn record(&self, status: StatusCode) {
        if let Some(metrics) = self.metrics {
            metrics.record(status, self.started.elapsed());
        }
    }
}
