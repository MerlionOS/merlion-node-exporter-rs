//! `/proc/stat` — per-CPU time spent in each mode.
//!
//! Emits two metric families, wire-compatible with upstream
//! `node_exporter`:
//!
//! - `node_cpu_seconds_total{cpu, mode}` counter — modes user / nice /
//!   system / idle / iowait / irq / softirq / steal.
//! - `node_cpu_guest_seconds_total{cpu, mode}` counter — modes user /
//!   nice. Only emitted when /proc/stat exposes the guest fields (kernels
//!   ≥ 2.6.24 — i.e., every supported system).
//!
//! Reference: `node_exporter/collector/cpu_linux.go` `Update()` and
//! `cpu_common.go:27` for the descriptor.

use std::fs;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

/// Linux scheduler tick rate. Upstream `node_exporter` hardcodes the
/// procfs assumption that `_SC_CLK_TCK` is 100 on every supported
/// platform; matching that here keeps the exported sample values
/// byte-identical for any normal kernel.
const USER_HZ: f64 = 100.0;

/// Mode label values for `node_cpu_seconds_total`, in the order
/// upstream emits them (the actual on-disk order depends on the text
/// serializer's sorting, but the *source* order matches this).
const MAIN_MODES: &[&str] = &[
    "user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal",
];

/// Mode label values for `node_cpu_guest_seconds_total`. /proc/stat
/// records ten fields per CPU; positions 8 and 9 are guest and `guest_nice`.
const GUEST_MODES: &[&str] = &["user", "nice"];

pub struct CpuCollector;

impl Collector for CpuCollector {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let path = cfg.proc_path("/proc/stat");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        parse(&raw)
    }
}

// jiffies → seconds via f64. u64 to f64 can lose precision past 2^53,
// but at 100 Hz that's still ~2.8 million years per CPU. Annotate
// rather than work around it.
#[allow(clippy::cast_precision_loss)]
fn parse(raw: &str) -> anyhow::Result<Vec<Metric>> {
    let mut main_metric = Metric::new(
        "node_cpu_seconds_total",
        "Seconds the CPUs spent in each mode.",
        MetricType::Counter,
    );
    let mut guest_metric = Metric::new(
        "node_cpu_guest_seconds_total",
        "Seconds the CPUs spent in guests (VMs) for each mode.",
        MetricType::Counter,
    );

    let mut saw_any_cpu = false;

    for line in raw.lines() {
        // Per-CPU rows look like "cpu0 ...", "cpu1 ...", etc. The aggregate
        // "cpu " row is intentionally skipped — upstream emits per-CPU only.
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        // First byte after "cpu" must be a digit. Anything else (including
        // a space, for the aggregate row) is not a per-CPU row.
        let first_byte = rest.as_bytes().first().copied().unwrap_or(b' ');
        if !first_byte.is_ascii_digit() {
            continue;
        }

        let mut fields = rest.split_ascii_whitespace();
        let cpu_id = fields
            .next()
            .ok_or_else(|| anyhow!("cpu: missing cpu id token"))?;

        let jiffies: Vec<u64> = fields
            .take(10)
            .enumerate()
            .map(|(i, s)| {
                s.parse::<u64>()
                    .with_context(|| format!("cpu: parsing field {i} for cpu{cpu_id}"))
            })
            .collect::<Result<_, _>>()?;

        if jiffies.len() < MAIN_MODES.len() {
            return Err(anyhow!(
                "cpu: cpu{cpu_id} has {} fields, need at least {}",
                jiffies.len(),
                MAIN_MODES.len()
            ));
        }
        saw_any_cpu = true;

        for (i, mode) in MAIN_MODES.iter().enumerate() {
            let seconds = jiffies[i] as f64 / USER_HZ;
            main_metric.push(
                Sample::new(seconds)
                    .with_label("cpu", cpu_id)
                    .with_label("mode", *mode),
            );
        }

        for (i, mode) in GUEST_MODES.iter().enumerate() {
            if let Some(&j) = jiffies.get(MAIN_MODES.len() + i) {
                let seconds = j as f64 / USER_HZ;
                guest_metric.push(
                    Sample::new(seconds)
                        .with_label("cpu", cpu_id)
                        .with_label("mode", *mode),
                );
            }
        }
    }

    if !saw_any_cpu {
        return Err(anyhow!("cpu: no per-CPU rows in /proc/stat"));
    }

    let mut out = vec![main_metric];
    if !guest_metric.samples.is_empty() {
        out.push(guest_metric);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_WITH_GUEST: &str = "\
cpu  100 200 300 400 500 600 700 800 900 1000
cpu0 10 20 30 40 50 60 70 80 90 100
cpu1 11 21 31 41 51 61 71 81 91 101
intr 12345 0 0
ctxt 67890
btime 1700000000
processes 42
procs_running 1
procs_blocked 0
softirq 99 0 0
";

    const FIXTURE_LEGACY_NO_GUEST: &str = "\
cpu  100 200 300 400 500 600 700 800
cpu0 10 20 30 40 50 60 70 80
";

    #[test]
    fn parses_with_guest_fields() {
        let metrics = parse(FIXTURE_WITH_GUEST).unwrap();
        assert_eq!(metrics.len(), 2, "main + guest");

        let main = &metrics[0];
        assert_eq!(main.name, "node_cpu_seconds_total");
        assert_eq!(main.mtype, MetricType::Counter);
        // 2 CPUs × 8 modes.
        assert_eq!(main.samples.len(), 16);

        // First per-CPU sample: cpu0 user = 10 / 100 = 0.1.
        let s = &main.samples[0];
        assert_eq!(s.labels[0], ("cpu".to_string(), "0".to_string()));
        assert_eq!(s.labels[1], ("mode".to_string(), "user".to_string()));
        assert!((s.value - 0.1).abs() < 1e-9);

        // cpu1 steal = 81 / 100 = 0.81. Index: cpu1's 8th mode = sample 15.
        let steal = &main.samples[15];
        assert_eq!(steal.labels[0].1, "1");
        assert_eq!(steal.labels[1].1, "steal");
        assert!((steal.value - 0.81).abs() < 1e-9);

        // Guest metric: 2 CPUs × 2 modes.
        let guest = &metrics[1];
        assert_eq!(guest.name, "node_cpu_guest_seconds_total");
        assert_eq!(guest.samples.len(), 4);
        // cpu0 guest user = position 8 = 90 / 100 = 0.9.
        let gs = &guest.samples[0];
        assert_eq!(gs.labels[0].1, "0");
        assert_eq!(gs.labels[1].1, "user");
        assert!((gs.value - 0.9).abs() < 1e-9);
    }

    #[test]
    fn skips_aggregate_cpu_row() {
        let metrics = parse(FIXTURE_WITH_GUEST).unwrap();
        let main = &metrics[0];
        // Aggregate "cpu " row must NOT appear — only cpu0 and cpu1. No
        // sample should have cpu="cpu" or empty.
        for s in &main.samples {
            assert!(s.labels[0].1 == "0" || s.labels[0].1 == "1");
        }
    }

    #[test]
    fn omits_guest_metric_when_fields_absent() {
        let metrics = parse(FIXTURE_LEGACY_NO_GUEST).unwrap();
        assert_eq!(metrics.len(), 1, "no guest metric on legacy kernel");
        assert_eq!(metrics[0].name, "node_cpu_seconds_total");
        assert_eq!(metrics[0].samples.len(), 8); // 1 CPU × 8 modes
    }

    #[test]
    fn rejects_input_with_no_cpu_rows() {
        let err = parse("intr 0\nctxt 0\n").unwrap_err().to_string();
        assert!(err.contains("no per-CPU rows"), "{err}");
    }

    #[test]
    fn rejects_truncated_cpu_row() {
        // cpu0 with only 5 fields — fewer than the 8 main modes.
        let err = parse("cpu0 1 2 3 4 5\n").unwrap_err().to_string();
        assert!(err.contains("need at least"), "{err}");
    }

    #[test]
    fn rejects_non_integer_field() {
        let err = parse("cpu0 1 2 banana 4 5 6 7 8\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("parsing field"), "{err}");
    }

    #[test]
    fn ignores_non_cpu_lines() {
        let raw = "\
btime 1700000000
cpu0 10 20 30 40 50 60 70 80
weird-line-just-for-fun
intr 12 0 0
";
        let metrics = parse(raw).unwrap();
        assert_eq!(metrics[0].samples.len(), 8);
    }
}
