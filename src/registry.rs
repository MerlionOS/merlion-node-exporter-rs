//! Collector trait and per-scrape registry.
//!
//! Each registered collector is invoked on every `/metrics` scrape and may
//! return either a populated `Vec<Metric>` or a structured error. Errors are
//! logged and surfaced as a `node_scrape_collector_success{collector="..."} 0`
//! sample so operators can alert on partial scrape failures — matching the
//! semantics of upstream `node_exporter`.

use std::time::Instant;

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};

/// A pluggable source of metric families.
pub trait Collector: Send + Sync {
    /// Stable identifier used for the `--no-collector.<name>` flag and for
    /// the `collector` label on the scrape-status metric.
    fn name(&self) -> &'static str;

    /// Read the underlying source and return zero or more metric families.
    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>>;
}

/// Collection of enabled collectors plus per-scrape orchestration.
pub struct Registry {
    collectors: Vec<Box<dyn Collector>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            collectors: Vec::new(),
        }
    }

    pub fn register(&mut self, c: Box<dyn Collector>) {
        self.collectors.push(c);
    }

    pub fn enabled_names(&self) -> Vec<&'static str> {
        self.collectors.iter().map(|c| c.name()).collect()
    }

    /// Run every collector and produce a single flat metric list, plus
    /// per-collector success/duration metrics for observability.
    pub fn gather(&self, cfg: &Config) -> Vec<Metric> {
        let mut out: Vec<Metric> = Vec::with_capacity(self.collectors.len() * 4);

        let mut success = Metric::new(
            "node_scrape_collector_success",
            "node_exporter: Whether a collector succeeded.",
            MetricType::Gauge,
        );
        let mut duration = Metric::new(
            "node_scrape_collector_duration_seconds",
            "node_exporter: Duration of a collector scrape.",
            MetricType::Gauge,
        );

        for c in &self.collectors {
            let started = Instant::now();
            let result = c.collect(cfg);
            let elapsed = started.elapsed().as_secs_f64();

            match result {
                Ok(metrics) => {
                    out.extend(metrics);
                    success.push(Sample::new(1.0).with_label("collector", c.name()));
                }
                Err(e) => {
                    tracing::error!(collector = c.name(), error = %e, "collector failed");
                    success.push(Sample::new(0.0).with_label("collector", c.name()));
                }
            }
            duration.push(Sample::new(elapsed).with_label("collector", c.name()));
        }

        out.push(success);
        out.push(duration);
        out
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
