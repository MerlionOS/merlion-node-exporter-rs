//! `/proc/meminfo` — system memory statistics.
//!
//! Output convention matches upstream `node_exporter`: every key in
//! `/proc/meminfo` becomes a metric `node_memory_<Key>_bytes` (or
//! `..._total` for unit-less keys like `HugePages_Total`). Values reported
//! in kB by the kernel are converted to bytes.

use std::fs;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct MeminfoCollector;

impl Collector for MeminfoCollector {
    fn name(&self) -> &'static str {
        "meminfo"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let path = cfg.proc_path("/proc/meminfo");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        parse(&raw)
    }
}

fn parse(raw: &str) -> anyhow::Result<Vec<Metric>> {
    let mut out = Vec::with_capacity(64);
    for (lineno, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (key, rest) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("meminfo: line {} missing ':'", lineno + 1))?;
        let mut parts = rest.split_ascii_whitespace();
        let value_str = parts
            .next()
            .ok_or_else(|| anyhow!("meminfo: line {} missing value", lineno + 1))?;
        let unit = parts.next();
        let raw_value: f64 = value_str
            .parse()
            .with_context(|| format!("meminfo: parsing value for {key}"))?;

        let (suffix, value) = match unit {
            Some("kB") => ("_bytes", raw_value * 1024.0),
            None => ("_total", raw_value),
            Some(other) => {
                return Err(anyhow!("meminfo: unexpected unit {other:?} for {key}"));
            }
        };

        let name = format!("node_memory_{}{}", sanitize(key), suffix);
        let help = format!("Memory information field {key}.");
        out.push(Metric::new(name, help, MetricType::Gauge).with_sample(Sample::new(value)));
    }
    if out.is_empty() {
        return Err(anyhow!("meminfo: no values parsed"));
    }
    Ok(out)
}

fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
MemTotal:       16384000 kB
MemFree:         1024000 kB
HugePages_Total:       0
HugePages_Free:        0
";

    #[test]
    fn parses_known_keys() {
        let metrics = parse(FIXTURE).unwrap();
        let total = metrics
            .iter()
            .find(|m| m.name == "node_memory_MemTotal_bytes")
            .unwrap();
        assert!((total.samples[0].value - 16_384_000.0 * 1024.0).abs() < 1.0);

        let hp = metrics
            .iter()
            .find(|m| m.name == "node_memory_HugePages_Total_total")
            .unwrap();
        assert!(hp.samples[0].value.abs() < f64::EPSILON);
    }

    #[test]
    fn sanitises_unusual_keys() {
        let metrics = parse("Some-Weird.Key:    42 kB\n").unwrap();
        assert_eq!(metrics[0].name, "node_memory_Some_Weird_Key_bytes");
    }

    #[test]
    fn errors_on_garbage() {
        assert!(parse("not a meminfo line\n").is_err());
    }
}
