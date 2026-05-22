//! Minimal metric model.
//!
//! We deliberately do not depend on a Prometheus client library: an exporter
//! re-reads its sources on every scrape, so the typed-counter / typed-gauge
//! pattern offered by `prometheus-client` is more verbose than it is useful
//! here. A flat `(name, labels, value)` model maps cleanly to /proc parsing
//! and serialises to the text exposition format in well under 100 lines.

use std::fmt;

/// Prometheus metric type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricType {
    Counter,
    Gauge,
    Untyped,
}

impl fmt::Display for MetricType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Untyped => "untyped",
        })
    }
}

/// One time-series sample within a metric family.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Label name/value pairs. Order is preserved as supplied by the collector.
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    pub fn new(value: f64) -> Self {
        Self {
            labels: Vec::new(),
            value,
        }
    }

    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.labels.push((key.into(), val.into()));
        self
    }
}

/// A metric family: name + HELP/TYPE metadata + one or more samples.
#[derive(Debug, Clone)]
pub struct Metric {
    pub name: String,
    pub help: String,
    pub mtype: MetricType,
    pub samples: Vec<Sample>,
}

impl Metric {
    pub fn new(name: impl Into<String>, help: impl Into<String>, mtype: MetricType) -> Self {
        Self {
            name: name.into(),
            help: help.into(),
            mtype,
            samples: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_sample(mut self, sample: Sample) -> Self {
        self.samples.push(sample);
        self
    }

    pub fn push(&mut self, sample: Sample) {
        self.samples.push(sample);
    }
}
