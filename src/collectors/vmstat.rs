//! `/proc/vmstat` — virtual memory subsystem statistics.
//!
//! Each line is a whitespace-separated `<key> <value>` pair. Every field
//! is exposed as `node_vmstat_<key>` with HELP text matching upstream
//! `node_exporter`'s `'/proc/vmstat information field <key>.'`.
//!
//! Type rule: keys starting with `nr_` describe an instantaneous
//! population count (free pages, dirty pages, …) and are emitted as
//! gauges. Everything else (`pgfault`, `pgpgin`, `oom_kill`, …) is a
//! monotonic event counter. This matches the spirit of upstream's
//! `UntypedValue` while giving Prometheus the correct semantic type.
//!
//! Divergence from upstream: upstream filters fields through a regexp
//! (`^(oom_kill|pgpg|pswp|pg.*fault).*` by default). The MVP emits every
//! field — the full set is ~100 lines and the filter can land later if
//! scrape size becomes a concern.

use std::fs;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct VmstatCollector;

impl Collector for VmstatCollector {
    fn name(&self) -> &'static str {
        "vmstat"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let path = cfg.proc_path("/proc/vmstat");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        parse(&raw)
    }
}

// u64 → f64 cast: vmstat counters can in principle exceed 2^53, but
// `pgfault` etc. would need decades of uptime on a single host to get
// near that. Annotate rather than work around.
#[allow(clippy::cast_precision_loss)]
fn parse(raw: &str) -> anyhow::Result<Vec<Metric>> {
    let mut out = Vec::with_capacity(128);
    for (lineno, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_ascii_whitespace();
        let key = parts
            .next()
            .ok_or_else(|| anyhow!("vmstat: line {} missing key", lineno + 1))?;
        let value_str = parts
            .next()
            .ok_or_else(|| anyhow!("vmstat: line {} missing value for {}", lineno + 1, key))?;
        let value: u64 = value_str
            .parse()
            .with_context(|| format!("vmstat: parsing value for {key}"))?;

        let mtype = if key.starts_with("nr_") {
            MetricType::Gauge
        } else {
            MetricType::Counter
        };

        let name = format!("node_vmstat_{key}");
        let help = format!("/proc/vmstat information field {key}.");
        out.push(Metric::new(name, help, mtype).with_sample(Sample::new(value as f64)));
    }
    if out.is_empty() {
        return Err(anyhow!("vmstat: no values parsed"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
nr_free_pages 12345
nr_zone_inactive_anon 678
nr_dirty 9
pgpgin 100000
pgpgout 200000
pgfault 67890
pgmajfault 42
oom_kill 1
";

    #[test]
    fn parses_counter_and_gauge_fields() {
        let metrics = parse(FIXTURE).unwrap();
        assert_eq!(metrics.len(), 8);

        let free = metrics
            .iter()
            .find(|m| m.name == "node_vmstat_nr_free_pages")
            .unwrap();
        assert_eq!(free.mtype, MetricType::Gauge);
        assert_eq!(free.help, "/proc/vmstat information field nr_free_pages.");
        assert!((free.samples[0].value - 12345.0).abs() < f64::EPSILON);

        let dirty = metrics
            .iter()
            .find(|m| m.name == "node_vmstat_nr_dirty")
            .unwrap();
        assert_eq!(dirty.mtype, MetricType::Gauge);

        let pgfault = metrics
            .iter()
            .find(|m| m.name == "node_vmstat_pgfault")
            .unwrap();
        assert_eq!(pgfault.mtype, MetricType::Counter);
        assert_eq!(pgfault.help, "/proc/vmstat information field pgfault.");
        assert!((pgfault.samples[0].value - 67890.0).abs() < f64::EPSILON);

        let oom = metrics
            .iter()
            .find(|m| m.name == "node_vmstat_oom_kill")
            .unwrap();
        assert_eq!(oom.mtype, MetricType::Counter);
        assert!((oom.samples[0].value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn skips_blank_lines() {
        let raw = "\npgfault 1\n\nnr_free_pages 2\n";
        let metrics = parse(raw).unwrap();
        assert_eq!(metrics.len(), 2);
    }

    #[test]
    fn rejects_non_integer_value() {
        let err = parse("pgfault banana\n").unwrap_err().to_string();
        assert!(err.contains("parsing value for pgfault"), "{err}");
    }

    #[test]
    fn rejects_line_missing_value() {
        let err = parse("pgfault\n").unwrap_err().to_string();
        assert!(err.contains("missing value"), "{err}");
    }

    #[test]
    fn rejects_empty_input() {
        let err = parse("").unwrap_err().to_string();
        assert!(err.contains("no values parsed"), "{err}");
    }
}
