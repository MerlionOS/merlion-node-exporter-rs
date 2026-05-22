//! `/proc/loadavg` — 1/5/15 minute load averages.

use std::fs;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct LoadavgCollector;

impl Collector for LoadavgCollector {
    fn name(&self) -> &'static str {
        "loadavg"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let path = cfg.proc_path("/proc/loadavg");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        parse(&raw)
    }
}

fn parse(raw: &str) -> anyhow::Result<Vec<Metric>> {
    let line = raw
        .lines()
        .next()
        .ok_or_else(|| anyhow!("loadavg: empty file"))?;
    let mut fields = line.split_ascii_whitespace();
    let one: f64 = field(&mut fields, "load1")?;
    let five: f64 = field(&mut fields, "load5")?;
    let fifteen: f64 = field(&mut fields, "load15")?;

    let make = |name: &str, help: &str, v: f64| {
        Metric::new(name, help, MetricType::Gauge).with_sample(Sample::new(v))
    };

    Ok(vec![
        make("node_load1", "1m load average.", one),
        make("node_load5", "5m load average.", five),
        make("node_load15", "15m load average.", fifteen),
    ])
}

fn field<'a>(iter: &mut impl Iterator<Item = &'a str>, name: &str) -> anyhow::Result<f64> {
    iter.next()
        .ok_or_else(|| anyhow!("loadavg: missing field {name}"))?
        .parse()
        .with_context(|| format!("loadavg: parsing {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_loadavg() {
        let raw = "0.34 0.40 0.41 1/1024 12345\n";
        let metrics = parse(raw).unwrap();
        assert_eq!(metrics.len(), 3);
        assert_eq!(metrics[0].name, "node_load1");
        assert!((metrics[0].samples[0].value - 0.34).abs() < 1e-9);
        assert!((metrics[2].samples[0].value - 0.41).abs() < 1e-9);
    }

    #[test]
    fn errors_on_empty_input() {
        assert!(parse("").is_err());
    }

    #[test]
    fn errors_on_truncated_input() {
        assert!(parse("0.1 0.2\n").is_err());
    }
}
