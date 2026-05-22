//! `/proc/pressure/{cpu,memory,io}` — Pressure Stall Information.
//!
//! PSI is a Linux ≥ 4.20 feature (requires `CONFIG_PSI=y` and, on some
//! distributions, `psi=1` on the kernel command line). Each resource
//! file looks like:
//!
//! ```text
//! some avg10=0.00 avg60=0.00 avg300=0.00 total=12345
//! full avg10=0.00 avg60=0.00 avg300=0.00 total=67890
//! ```
//!
//! `/proc/pressure/cpu` carries only the `some` line; `memory` and `io`
//! carry both. We expose two counters per resource, matching upstream
//! `node_exporter/collector/pressure_linux.go`:
//!
//! - `node_pressure_<resource>_waiting_seconds_total` — from `some`'s `total=`.
//! - `node_pressure_<resource>_stalled_seconds_total` — from `full`'s `total=`,
//!   emitted for `memory` and `io` only.
//!
//! The `total=` field is microseconds; we divide by 1e6 to get seconds.
//!
//! If any of the three files is missing we skip that resource silently
//! (older kernel, `CONFIG_PSI` not enabled). Only when *none* of the
//! files exist does the collector return an error, so per-collector
//! `success=0` correctly reflects "PSI unavailable on this host".

use std::fs;
use std::io::ErrorKind;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

/// One PSI resource we read. The third field is whether the file carries
/// a `full` line (true for memory/io, false for cpu).
const RESOURCES: &[(&str, &str, bool)] = &[
    ("cpu", "/proc/pressure/cpu", false),
    ("io", "/proc/pressure/io", true),
    ("memory", "/proc/pressure/memory", true),
];

pub struct PressureCollector;

impl Collector for PressureCollector {
    fn name(&self) -> &'static str {
        "pressure"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let mut out: Vec<Metric> = Vec::with_capacity(5);
        let mut found = 0usize;

        for (resource, rel_path, expect_full) in RESOURCES {
            let path = cfg.proc_path(rel_path);
            let raw = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    // Kernel < 4.20 or CONFIG_PSI=n — skip silently.
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("reading {}", path.display()));
                }
            };
            found += 1;

            let (some_total_us, full_total_us) =
                parse(&raw).with_context(|| format!("parsing {}", path.display()))?;

            out.push(waiting_metric(resource, some_total_us));

            if *expect_full {
                let full = full_total_us.ok_or_else(|| {
                    anyhow!("pressure: {} missing required 'full' line", path.display())
                })?;
                out.push(stalled_metric(resource, full));
            }
        }

        if found == 0 {
            return Err(anyhow!(
                "pressure: no /proc/pressure/* files present (kernel < 4.20 or CONFIG_PSI=n)"
            ));
        }

        Ok(out)
    }
}

// Microseconds → seconds via f64. u64 → f64 loses precision past 2^53 µs
// (~285 years), which is well outside any realistic uptime.
#[allow(clippy::cast_precision_loss)]
fn us_to_seconds(us: u64) -> f64 {
    us as f64 / 1_000_000.0
}

fn waiting_metric(resource: &str, total_us: u64) -> Metric {
    // HELP text mirrors upstream node_exporter pressure_linux.go.
    let help = match resource {
        "cpu" => "Total time in seconds that processes have waited for CPU time",
        "io" => "Total time in seconds that processes have waited due to IO congestion",
        "memory" => "Total time in seconds that processes have waited for memory",
        _ => "Total time in seconds that processes have waited for this resource",
    };
    Metric::new(
        format!("node_pressure_{resource}_waiting_seconds_total"),
        help,
        MetricType::Counter,
    )
    .with_sample(Sample::new(us_to_seconds(total_us)))
}

fn stalled_metric(resource: &str, total_us: u64) -> Metric {
    // HELP text mirrors upstream node_exporter pressure_linux.go.
    let help = match resource {
        "io" => "Total time in seconds no process could make progress due to IO congestion",
        "memory" => "Total time in seconds no process could make progress due to memory congestion",
        _ => "Total time in seconds no process could make progress due to congestion",
    };
    Metric::new(
        format!("node_pressure_{resource}_stalled_seconds_total"),
        help,
        MetricType::Counter,
    )
    .with_sample(Sample::new(us_to_seconds(total_us)))
}

/// Parse one /proc/pressure/* file.
///
/// Returns `(some_total_us, full_total_us)`. `full_total_us` is `Some`
/// when a `full` line was present (memory/io), `None` otherwise (cpu).
///
/// Errors if the `some` line is absent, if a line cannot be tokenised,
/// or if a present line is missing its `total=` field.
pub fn parse(raw: &str) -> anyhow::Result<(u64, Option<u64>)> {
    let mut some_total: Option<u64> = None;
    let mut full_total: Option<u64> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut fields = line.split_ascii_whitespace();
        let kind = fields
            .next()
            .ok_or_else(|| anyhow!("pressure: empty line"))?;

        match kind {
            "some" => some_total = Some(extract_total(fields, "some")?),
            "full" => full_total = Some(extract_total(fields, "full")?),
            other => {
                return Err(anyhow!("pressure: unknown line kind {other:?}"));
            }
        }
    }

    let some = some_total.ok_or_else(|| anyhow!("pressure: missing 'some' line"))?;
    Ok((some, full_total))
}

/// Walk the remaining tokens of a PSI line and return the `total=` value.
fn extract_total<'a>(fields: impl Iterator<Item = &'a str>, kind: &str) -> anyhow::Result<u64> {
    for tok in fields {
        if let Some(v) = tok.strip_prefix("total=") {
            return v
                .parse::<u64>()
                .with_context(|| format!("pressure: parsing total= on {kind} line"));
        }
    }
    Err(anyhow!("pressure: missing total= on {kind} line"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CPU_FIXTURE: &str = "\
some avg10=0.00 avg60=0.00 avg300=0.00 total=123456789
";

    const MEMORY_FIXTURE: &str = "\
some avg10=0.10 avg60=0.20 avg300=0.30 total=2000000
full avg10=0.05 avg60=0.10 avg300=0.15 total=1000000
";

    const IO_FIXTURE: &str = "\
some avg10=1.00 avg60=2.00 avg300=3.00 total=5000000
full avg10=0.50 avg60=1.00 avg300=1.50 total=2500000
";

    #[test]
    fn parses_cpu_some_only() {
        let (some, full) = parse(CPU_FIXTURE).unwrap();
        assert_eq!(some, 123_456_789);
        assert!(full.is_none(), "cpu has no full line");
    }

    #[test]
    fn parses_memory_some_and_full() {
        let (some, full) = parse(MEMORY_FIXTURE).unwrap();
        assert_eq!(some, 2_000_000);
        assert_eq!(full, Some(1_000_000));
    }

    #[test]
    fn parses_io_some_and_full() {
        let (some, full) = parse(IO_FIXTURE).unwrap();
        assert_eq!(some, 5_000_000);
        assert_eq!(full, Some(2_500_000));
    }

    #[test]
    fn rejects_missing_some_line() {
        // full-only — invalid; PSI always reports `some` when `full` is present.
        let raw = "full avg10=0.00 avg60=0.00 avg300=0.00 total=42\n";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("missing 'some'"), "{err}");
    }

    #[test]
    fn rejects_missing_total_field() {
        let raw = "some avg10=0.00 avg60=0.00 avg300=0.00\n";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("missing total="), "{err}");
    }

    #[test]
    fn rejects_unknown_line_kind() {
        let raw = "weird avg10=0.00 total=1\n";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("unknown line kind"), "{err}");
    }

    #[test]
    fn rejects_unparseable_total() {
        let raw = "some avg10=0.00 avg60=0.00 avg300=0.00 total=banana\n";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("parsing total="), "{err}");
    }

    #[test]
    fn microseconds_convert_to_seconds() {
        // 2_500_000 µs == 2.5 s. Exercise the actual metric builder so a
        // future refactor of the conversion factor can't silently drift.
        let m = waiting_metric("io", 2_500_000);
        assert_eq!(m.name, "node_pressure_io_waiting_seconds_total");
        assert_eq!(m.mtype, MetricType::Counter);
        assert!((m.samples[0].value - 2.5).abs() < 1e-9);

        let s = stalled_metric("memory", 1_000_000);
        assert_eq!(s.name, "node_pressure_memory_stalled_seconds_total");
        assert!((s.samples[0].value - 1.0).abs() < 1e-9);
    }

    #[test]
    fn collect_against_fixture_tree() {
        // End-to-end: write a fake procfs with all three files, run the
        // collector, and check it emits 5 metrics (cpu waiting; io
        // waiting + stalled; memory waiting + stalled) in the order we
        // declare RESOURCES.
        let tmp = tempfile::tempdir().unwrap();
        let pressure_dir = tmp.path().join("pressure");
        std::fs::create_dir_all(&pressure_dir).unwrap();
        std::fs::write(pressure_dir.join("cpu"), CPU_FIXTURE).unwrap();
        std::fs::write(pressure_dir.join("io"), IO_FIXTURE).unwrap();
        std::fs::write(pressure_dir.join("memory"), MEMORY_FIXTURE).unwrap();

        let cfg = Config::new(tmp.path().to_path_buf(), "/sys".into(), "/".into());
        let metrics = PressureCollector.collect(&cfg).unwrap();

        let names: Vec<_> = metrics.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "node_pressure_cpu_waiting_seconds_total",
                "node_pressure_io_waiting_seconds_total",
                "node_pressure_io_stalled_seconds_total",
                "node_pressure_memory_waiting_seconds_total",
                "node_pressure_memory_stalled_seconds_total",
            ]
        );

        // cpu waiting = 123_456_789 µs ≈ 123.456789 s
        assert!((metrics[0].samples[0].value - 123.456_789).abs() < 1e-6);
    }

    #[test]
    fn collect_tolerates_missing_resource_files() {
        // Only cpu present — collector should still succeed with just
        // the cpu waiting metric.
        let tmp = tempfile::tempdir().unwrap();
        let pressure_dir = tmp.path().join("pressure");
        std::fs::create_dir_all(&pressure_dir).unwrap();
        std::fs::write(pressure_dir.join("cpu"), CPU_FIXTURE).unwrap();

        let cfg = Config::new(tmp.path().to_path_buf(), "/sys".into(), "/".into());
        let metrics = PressureCollector.collect(&cfg).unwrap();

        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "node_pressure_cpu_waiting_seconds_total");
    }

    #[test]
    fn collect_errors_when_no_files_present() {
        // Empty fixture tree — PSI not available on this kernel.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::new(tmp.path().to_path_buf(), "/sys".into(), "/".into());
        let err = PressureCollector.collect(&cfg).unwrap_err().to_string();
        assert!(err.contains("no /proc/pressure/* files"), "{err}");
    }
}
