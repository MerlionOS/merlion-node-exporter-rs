//! `/proc/stat` — kernel and system statistics (non-CPU lines).
//!
//! The CPU jiffy lines (`cpu` aggregate and `cpuN` per-core) are owned
//! by the [`cpu`](super::cpu) collector. This collector handles the
//! remaining keyword lines that upstream `node_exporter`'s
//! `stat_linux.go` emits:
//!
//! - `node_boot_time_seconds` (gauge) — `btime <unixtime>`.
//! - `node_intr_total` (counter) — first integer on the `intr` line
//!   (total interrupts; the per-IRQ breakdown that follows is dropped
//!   to stay aligned with upstream defaults).
//! - `node_context_switches_total` (counter) — `ctxt <n>`.
//! - `node_forks_total` (counter) — `processes <n>`. The kernel line
//!   is called `processes`; upstream renames the *metric* to
//!   `forks_total` because that's what the counter actually measures.
//! - `node_procs_running` (gauge) — `procs_running <n>`.
//! - `node_procs_blocked` (gauge) — `procs_blocked <n>`.
//! - `node_softirqs_total` (counter) — first integer on the `softirq`
//!   line (total softirq calls; per-vector breakdown is gated behind
//!   `--collector.stat.softirq` upstream and is out of MVP scope).
//!
//! Reference: `node_exporter/collector/stat_linux.go`.

use std::fs;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct StatCollector;

impl Collector for StatCollector {
    fn name(&self) -> &'static str {
        "stat"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let path = cfg.proc_path("/proc/stat");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        parse(&raw)
    }
}

fn parse(raw: &str) -> anyhow::Result<Vec<Metric>> {
    let mut btime: Option<u64> = None;
    let mut intr: Option<u64> = None;
    let mut ctxt: Option<u64> = None;
    let mut forks: Option<u64> = None;
    let mut procs_running: Option<u64> = None;
    let mut procs_blocked: Option<u64> = None;
    let mut softirq: Option<u64> = None;

    for line in raw.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(key) = fields.next() else {
            continue;
        };
        match key {
            "btime" => btime = Some(first_u64(&mut fields, "btime")?),
            "intr" => intr = Some(first_u64(&mut fields, "intr")?),
            "ctxt" => ctxt = Some(first_u64(&mut fields, "ctxt")?),
            "processes" => forks = Some(first_u64(&mut fields, "processes")?),
            "procs_running" => procs_running = Some(first_u64(&mut fields, "procs_running")?),
            "procs_blocked" => procs_blocked = Some(first_u64(&mut fields, "procs_blocked")?),
            "softirq" => softirq = Some(first_u64(&mut fields, "softirq")?),
            // CPU rows are handled by the `cpu` collector; everything else
            // (unknown / future kernel keys) is intentionally ignored so we
            // keep working on newer kernels.
            _ => {}
        }
    }

    let btime = btime.ok_or_else(|| anyhow!("stat: missing btime line"))?;
    let intr = intr.ok_or_else(|| anyhow!("stat: missing intr line"))?;
    let ctxt = ctxt.ok_or_else(|| anyhow!("stat: missing ctxt line"))?;
    let forks = forks.ok_or_else(|| anyhow!("stat: missing processes line"))?;
    let procs_running = procs_running.ok_or_else(|| anyhow!("stat: missing procs_running line"))?;
    let procs_blocked = procs_blocked.ok_or_else(|| anyhow!("stat: missing procs_blocked line"))?;
    let softirq = softirq.ok_or_else(|| anyhow!("stat: missing softirq line"))?;

    Ok(vec![
        gauge(
            "node_boot_time_seconds",
            "Node boot time, in unixtime.",
            u64_to_f64(btime),
        ),
        counter(
            "node_intr_total",
            "Total number of interrupts serviced.",
            u64_to_f64(intr),
        ),
        counter(
            "node_context_switches_total",
            "Total number of context switches.",
            u64_to_f64(ctxt),
        ),
        counter(
            "node_forks_total",
            "Total number of forks.",
            u64_to_f64(forks),
        ),
        gauge(
            "node_procs_running",
            "Number of processes in runnable state.",
            u64_to_f64(procs_running),
        ),
        gauge(
            "node_procs_blocked",
            "Number of processes blocked waiting for I/O to complete.",
            u64_to_f64(procs_blocked),
        ),
        counter(
            "node_softirqs_total",
            "Number of softirq calls.",
            u64_to_f64(softirq),
        ),
    ])
}

fn first_u64<'a>(iter: &mut impl Iterator<Item = &'a str>, key: &str) -> anyhow::Result<u64> {
    let token = iter
        .next()
        .ok_or_else(|| anyhow!("stat: missing value for {key}"))?;
    token
        .parse::<u64>()
        .with_context(|| format!("stat: parsing {key} value {token:?}"))
}

fn gauge(name: &str, help: &str, v: f64) -> Metric {
    Metric::new(name, help, MetricType::Gauge).with_sample(Sample::new(v))
}

fn counter(name: &str, help: &str, v: f64) -> Metric {
    Metric::new(name, help, MetricType::Counter).with_sample(Sample::new(v))
}

// All seven values are unsigned 64-bit kernel counters. boot time fits in
// 32 bits comfortably; the rest can in principle exceed 2^53 on extremely
// long-lived hosts (years of uptime with millions of interrupts/sec) but
// in practice this never happens — and exposition is in f64 either way.
#[allow(clippy::cast_precision_loss)]
fn u64_to_f64(v: u64) -> f64 {
    v as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed-down version of the upstream fixture at
    // node_exporter/collector/fixtures/proc/stat. Per-CPU rows kept short;
    // intr/softirq retain their first-token-is-total shape with a couple of
    // per-vector values following.
    const FIXTURE: &str = "\
cpu  301854 612 111922 8979004 3552 2 3944 0 44 36
cpu0 44490 19 21045 1087069 220 1 3410 0 2 1
cpu1 47869 23 16474 1110787 591 0 46 0 3 2
intr 8885917 17 0 0 0 0 0 0 0 1 79281
ctxt 38014093
btime 1418183276
processes 26442
procs_running 2
procs_blocked 0
softirq 5057579 250191 1481983 1647 211099 186066 0 1783454 622196 12499 508444
";

    fn find<'a>(metrics: &'a [Metric], name: &str) -> &'a Metric {
        metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("metric {name} not found"))
    }

    #[test]
    fn parses_all_seven_metrics_from_realistic_fixture() {
        let metrics = parse(FIXTURE).unwrap();
        assert_eq!(metrics.len(), 7);

        let btime = find(&metrics, "node_boot_time_seconds");
        assert_eq!(btime.mtype, MetricType::Gauge);
        assert!((btime.samples[0].value - 1_418_183_276.0).abs() < f64::EPSILON);

        let intr = find(&metrics, "node_intr_total");
        assert_eq!(intr.mtype, MetricType::Counter);
        // Total only — not the sum of the per-IRQ tail.
        assert!((intr.samples[0].value - 8_885_917.0).abs() < f64::EPSILON);

        let ctxt = find(&metrics, "node_context_switches_total");
        assert_eq!(ctxt.mtype, MetricType::Counter);
        assert!((ctxt.samples[0].value - 38_014_093.0).abs() < f64::EPSILON);

        let forks = find(&metrics, "node_forks_total");
        assert_eq!(forks.mtype, MetricType::Counter);
        // Sourced from the `processes` line.
        assert!((forks.samples[0].value - 26_442.0).abs() < f64::EPSILON);

        let running = find(&metrics, "node_procs_running");
        assert_eq!(running.mtype, MetricType::Gauge);
        assert!((running.samples[0].value - 2.0).abs() < f64::EPSILON);

        let blocked = find(&metrics, "node_procs_blocked");
        assert_eq!(blocked.mtype, MetricType::Gauge);
        assert!(blocked.samples[0].value.abs() < f64::EPSILON);

        let softirq = find(&metrics, "node_softirqs_total");
        assert_eq!(softirq.mtype, MetricType::Counter);
        // Total only — first token after `softirq`.
        assert!((softirq.samples[0].value - 5_057_579.0).abs() < f64::EPSILON);
    }

    #[test]
    fn each_metric_has_exactly_one_sample_with_no_labels() {
        let metrics = parse(FIXTURE).unwrap();
        for m in &metrics {
            assert_eq!(m.samples.len(), 1, "{}", m.name);
            assert!(m.samples[0].labels.is_empty(), "{}", m.name);
        }
    }

    #[test]
    fn errors_when_btime_missing() {
        // Strip the btime line from the fixture.
        let raw = FIXTURE
            .lines()
            .filter(|l| !l.starts_with("btime"))
            .collect::<Vec<_>>()
            .join("\n");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("btime"), "{err}");
    }

    #[test]
    fn errors_when_softirq_missing() {
        let raw = FIXTURE
            .lines()
            .filter(|l| !l.starts_with("softirq"))
            .collect::<Vec<_>>()
            .join("\n");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("softirq"), "{err}");
    }

    #[test]
    fn errors_on_non_integer_intr_total() {
        let raw = "\
btime 1
intr banana
ctxt 1
processes 1
procs_running 0
procs_blocked 0
softirq 1
";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("parsing intr"), "{err}");
    }

    #[test]
    fn errors_on_empty_input() {
        assert!(parse("").is_err());
    }

    #[test]
    fn ignores_unknown_lines_and_cpu_rows() {
        // Add a hypothetical future keyword; parser must keep working.
        let raw = format!("{FIXTURE}future_key 999 1 2 3\n");
        let metrics = parse(&raw).unwrap();
        assert_eq!(metrics.len(), 7);
    }
}
