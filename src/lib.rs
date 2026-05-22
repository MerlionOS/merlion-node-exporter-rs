//! Library entrypoints for `merlion-node-exporter`.
//!
//! Most users will interact with the binary in `src/main.rs`; this crate is
//! also exposed as a library so collectors can be unit-tested in isolation
//! and so external consumers can embed the registry.

pub mod cli;
pub mod collectors;
pub mod config;
pub mod encoding;
pub mod metric;
pub mod registry;
pub mod server;

pub use config::Config;
pub use metric::{Metric, MetricType, Sample};
pub use registry::{Collector, Registry};
