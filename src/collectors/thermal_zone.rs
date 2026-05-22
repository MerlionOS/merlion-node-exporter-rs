//! `/sys/class/thermal/{thermal_zone,cooling_device}*` — kernel thermal
//! framework metrics.
//!
//! Emits three metric families, wire-compatible with upstream
//! `node_exporter`:
//!
//! - `node_thermal_zone_temp{zone, type}` gauge — zone temperature in
//!   degrees Celsius. The kernel reports millidegrees Celsius via
//!   `thermal_zone<N>/temp`; we divide by 1000.
//! - `node_cooling_device_cur_state{name, type}` gauge — current throttle
//!   state of the cooling device. Can legitimately be -1 (intel powerclamp).
//! - `node_cooling_device_max_state{name, type}` gauge — maximum throttle
//!   state of the cooling device.
//!
//! Per-entry parse failures are isolated: one malformed
//! `thermal_zone7/temp` will not fail the whole collector — the broken
//! zone is logged and skipped so the remaining zones still scrape.
//!
//! Reference: `node_exporter/collector/thermal_zone_linux.go` and
//! `prometheus/procfs/sysfs/class_{thermal,cooling_device}.go`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct ThermalZoneCollector;

impl Collector for ThermalZoneCollector {
    fn name(&self) -> &'static str {
        "thermal_zone"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        collect_from(&cfg.sys_path("/sys/class/thermal"))
    }
}

/// Read `/sys/class/thermal/{thermal_zone,cooling_device}*` from `root`
/// and produce the three metric families.
///
/// `root` is the directory that contains the per-zone / per-device
/// subdirectories — i.e. `<sysfs>/class/thermal`. Returning `Ok` with an
/// empty `Vec` would be wrong (it suggests success-with-no-data); we
/// always return all three metric families and let the encoder emit
/// HELP/TYPE for empty ones — Prometheus is happy with that.
fn collect_from(root: &Path) -> anyhow::Result<Vec<Metric>> {
    let mut zone_temp = Metric::new(
        "node_thermal_zone_temp",
        "Zone temperature in Celsius",
        MetricType::Gauge,
    );
    let mut cd_cur = Metric::new(
        "node_cooling_device_cur_state",
        "Current throttle state of the cooling device",
        MetricType::Gauge,
    );
    let mut cd_max = Metric::new(
        "node_cooling_device_max_state",
        "Maximum throttle state of the cooling device",
        MetricType::Gauge,
    );

    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No /sys/class/thermal — same disposition as upstream's
            // `ErrNoData`: degrade to an empty (but well-formed) result
            // so the per-collector success metric still flips to 1.
            return Ok(vec![zone_temp, cd_cur, cd_max]);
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("reading dir {}", root.display())));
        }
    };

    // Collect-and-sort so output is deterministic across filesystems
    // (kernel `read_dir` order is not stable across reboots).
    let mut zone_dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut cd_dirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("iterating {}", root.display()))?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(suffix) = name.strip_prefix("thermal_zone") {
            if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
                zone_dirs.push((suffix.to_string(), entry.path()));
            }
        } else if let Some(suffix) = name.strip_prefix("cooling_device") {
            if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
                cd_dirs.push((suffix.to_string(), entry.path()));
            }
        }
    }
    zone_dirs.sort_by_key(|a| natural_index(&a.0));
    cd_dirs.sort_by_key(|a| natural_index(&a.0));

    for (index, path) in &zone_dirs {
        match parse_zone(path) {
            Ok(zone) => {
                zone_temp.push(
                    Sample::new(millideg_to_celsius(zone.temp_millideg))
                        .with_label("zone", index.as_str())
                        .with_label("type", zone.zone_type),
                );
            }
            Err(e) => {
                tracing::debug!(zone = %index, path = %path.display(), error = %e,
                    "thermal_zone: skipping zone");
            }
        }
    }

    for (index, path) in &cd_dirs {
        match parse_cooling_device(path) {
            Ok(cd) => {
                cd_cur.push(
                    Sample::new(int_to_f64(cd.cur_state))
                        .with_label("name", index.as_str())
                        .with_label("type", cd.cd_type.clone()),
                );
                cd_max.push(
                    Sample::new(int_to_f64(cd.max_state))
                        .with_label("name", index.as_str())
                        .with_label("type", cd.cd_type),
                );
            }
            Err(e) => {
                tracing::debug!(device = %index, path = %path.display(), error = %e,
                    "thermal_zone: skipping cooling device");
            }
        }
    }

    Ok(vec![zone_temp, cd_cur, cd_max])
}

/// Parse the numeric trailing index off a `thermal_zone<N>` /
/// `cooling_device<N>` directory name. Used purely for deterministic
/// output ordering — falls back to `u64::MAX` for anything that doesn't
/// fit, which only matters if a sysfs directory carries a > 20-digit
/// index (it won't).
fn natural_index(s: &str) -> u64 {
    s.parse().unwrap_or(u64::MAX)
}

#[derive(Debug)]
struct ThermalZone {
    zone_type: String,
    temp_millideg: i64,
}

#[derive(Debug)]
struct CoolingDevice {
    cd_type: String,
    cur_state: i64,
    max_state: i64,
}

fn parse_zone(dir: &Path) -> anyhow::Result<ThermalZone> {
    // Required attributes — `type` and `temp`. `policy` is required by
    // the kernel ABI but not exported as a metric; we still read it so
    // we surface a clean error if the kernel hands us a malformed zone.
    let zone_type = read_trimmed(&dir.join("type"))?;
    let _policy = read_trimmed(&dir.join("policy"))?;
    let temp_millideg = read_int(&dir.join("temp"))?;

    // Optional `mode` — accept missing-or-unreadable silently. We don't
    // emit a metric for it (upstream node_exporter doesn't either), but
    // we still consume errors that indicate file presence so a future
    // mode-as-label change is a one-liner.
    let _mode = read_optional(&dir.join("mode"));

    Ok(ThermalZone {
        zone_type,
        temp_millideg,
    })
}

fn parse_cooling_device(dir: &Path) -> anyhow::Result<CoolingDevice> {
    let cd_type = read_trimmed(&dir.join("type"))?;
    let max_state = read_int(&dir.join("max_state"))?;
    // cur_state may legitimately be -1 (intel_powerclamp); signed parse
    // is required.
    let cur_state = read_int(&dir.join("cur_state"))?;
    Ok(CoolingDevice {
        cd_type,
        cur_state,
        max_state,
    })
}

/// Read a sysfs string attribute and trim trailing newline + whitespace
/// — sysfs files always end in `\n`.
fn read_trimmed(path: &Path) -> anyhow::Result<String> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(raw.trim().to_string())
}

/// Read a sysfs integer attribute. Signed because `cur_state` can be
/// negative; everything else is non-negative in practice.
fn read_int(path: &Path) -> anyhow::Result<i64> {
    let raw = read_trimmed(path)?;
    raw.parse::<i64>()
        .with_context(|| format!("parsing integer from {}: {raw:?}", path.display()))
}

/// Read an optional sysfs attribute — returns `None` if the file is
/// missing, otherwise the trimmed contents. Permission-denied is treated
/// the same as missing (consistent with the procfs Go library).
fn read_optional(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(s) => Some(s.trim().to_string()),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            None
        }
        Err(_) => None,
    }
}

// millidegree → degree as f64. Precision loss past 2^53 millidegrees is
// purely theoretical (~9e12 °C); annotating rather than fighting clippy.
#[allow(clippy::cast_precision_loss)]
fn millideg_to_celsius(millideg: i64) -> f64 {
    millideg as f64 / 1000.0
}

// `cur_state` / `max_state` are integer-valued gauges; the f64 model
// requires a cast. Same precision-loss caveat as above.
#[allow(clippy::cast_precision_loss)]
fn int_to_f64(v: i64) -> f64 {
    v as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;

    /// Build a fake `<sysfs>/class/thermal` directory tree and return its
    /// path along with the `TempDir` guard (drop = cleanup).
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().join("class/thermal");
        stdfs::create_dir_all(&root).unwrap();
        (td, root)
    }

    fn write_zone(root: &Path, n: u32, ty: &str, temp_millideg: i64, policy: &str) -> PathBuf {
        let dir = root.join(format!("thermal_zone{n}"));
        stdfs::create_dir_all(&dir).unwrap();
        stdfs::write(dir.join("type"), format!("{ty}\n")).unwrap();
        stdfs::write(dir.join("temp"), format!("{temp_millideg}\n")).unwrap();
        stdfs::write(dir.join("policy"), format!("{policy}\n")).unwrap();
        dir
    }

    fn write_cooling_device(
        root: &Path,
        n: u32,
        ty: &str,
        cur_state: i64,
        max_state: i64,
    ) -> PathBuf {
        let dir = root.join(format!("cooling_device{n}"));
        stdfs::create_dir_all(&dir).unwrap();
        stdfs::write(dir.join("type"), format!("{ty}\n")).unwrap();
        stdfs::write(dir.join("cur_state"), format!("{cur_state}\n")).unwrap();
        stdfs::write(dir.join("max_state"), format!("{max_state}\n")).unwrap();
        dir
    }

    fn find_metric<'a>(metrics: &'a [Metric], name: &str) -> &'a Metric {
        metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("metric {name} not found"))
    }

    #[test]
    fn happy_path_one_zone_one_cooling_device() {
        let (_td, root) = fixture();
        write_zone(&root, 0, "x86_pkg_temp", 45_000, "step_wise");
        write_cooling_device(&root, 0, "Processor", 1, 10);

        let metrics = collect_from(&root).unwrap();
        let temp = find_metric(&metrics, "node_thermal_zone_temp");
        assert_eq!(temp.mtype, MetricType::Gauge);
        assert_eq!(temp.samples.len(), 1);
        assert!((temp.samples[0].value - 45.0).abs() < 1e-9);
        assert_eq!(temp.samples[0].labels[0], ("zone".into(), "0".into()));
        assert_eq!(
            temp.samples[0].labels[1],
            ("type".into(), "x86_pkg_temp".into())
        );

        let cur = find_metric(&metrics, "node_cooling_device_cur_state");
        assert_eq!(cur.samples.len(), 1);
        assert!((cur.samples[0].value - 1.0).abs() < f64::EPSILON);
        assert_eq!(cur.samples[0].labels[0], ("name".into(), "0".into()));
        assert_eq!(
            cur.samples[0].labels[1],
            ("type".into(), "Processor".into())
        );

        let max = find_metric(&metrics, "node_cooling_device_max_state");
        assert_eq!(max.samples.len(), 1);
        assert!((max.samples[0].value - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_mode_file_is_not_an_error() {
        let (_td, root) = fixture();
        // No `mode` file deliberately written.
        write_zone(&root, 0, "acpitz", 38_500, "step_wise");

        let metrics = collect_from(&root).unwrap();
        let temp = find_metric(&metrics, "node_thermal_zone_temp");
        assert_eq!(temp.samples.len(), 1);
        assert!((temp.samples[0].value - 38.5).abs() < 1e-9);
    }

    #[test]
    fn mode_file_present_is_accepted() {
        let (_td, root) = fixture();
        let zone = write_zone(&root, 0, "acpitz", 50_000, "step_wise");
        stdfs::write(zone.join("mode"), "enabled\n").unwrap();

        let metrics = collect_from(&root).unwrap();
        let temp = find_metric(&metrics, "node_thermal_zone_temp");
        assert_eq!(temp.samples.len(), 1);
    }

    #[test]
    fn malformed_temp_isolates_to_that_zone() {
        let (_td, root) = fixture();
        write_zone(&root, 0, "x86_pkg_temp", 42_000, "step_wise");
        // Zone 1 — temp file is garbage. Collector must skip it but
        // still emit zone 0.
        let bad = root.join("thermal_zone1");
        stdfs::create_dir_all(&bad).unwrap();
        stdfs::write(bad.join("type"), "broken\n").unwrap();
        stdfs::write(bad.join("policy"), "step_wise\n").unwrap();
        stdfs::write(bad.join("temp"), "not-a-number\n").unwrap();

        let metrics = collect_from(&root).unwrap();
        let temp = find_metric(&metrics, "node_thermal_zone_temp");
        assert_eq!(temp.samples.len(), 1, "only zone 0 survives");
        assert_eq!(temp.samples[0].labels[0], ("zone".into(), "0".into()));
    }

    #[test]
    fn missing_thermal_root_returns_empty_families() {
        let td = tempfile::tempdir().unwrap();
        // Point at a path that doesn't exist.
        let metrics = collect_from(&td.path().join("nope/class/thermal")).unwrap();
        assert_eq!(metrics.len(), 3);
        for m in &metrics {
            assert!(m.samples.is_empty(), "{} has no samples", m.name);
        }
    }

    #[test]
    fn cooling_device_negative_cur_state_accepted() {
        let (_td, root) = fixture();
        // intel_powerclamp reports cur_state=-1 when idle injection is off.
        write_cooling_device(&root, 0, "intel_powerclamp", -1, 50);

        let metrics = collect_from(&root).unwrap();
        let cur = find_metric(&metrics, "node_cooling_device_cur_state");
        assert_eq!(cur.samples.len(), 1);
        assert!((cur.samples[0].value - -1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn multiple_zones_are_emitted_in_index_order() {
        let (_td, root) = fixture();
        // Write in non-sorted order to exercise the sort.
        write_zone(&root, 10, "type10", 10_000, "step_wise");
        write_zone(&root, 2, "type2", 2_000, "step_wise");
        write_zone(&root, 0, "type0", 0, "step_wise");

        let metrics = collect_from(&root).unwrap();
        let temp = find_metric(&metrics, "node_thermal_zone_temp");
        assert_eq!(temp.samples.len(), 3);
        let zones: Vec<&str> = temp
            .samples
            .iter()
            .map(|s| s.labels[0].1.as_str())
            .collect();
        assert_eq!(zones, vec!["0", "2", "10"]);
    }

    #[test]
    fn ignores_unrelated_directories() {
        let (_td, root) = fixture();
        // A subdirectory that looks similar but isn't a zone/device.
        stdfs::create_dir_all(root.join("thermal_zone_special")).unwrap();
        stdfs::create_dir_all(root.join("cooling_device_xyz")).unwrap();
        write_zone(&root, 0, "acpitz", 30_000, "step_wise");

        let metrics = collect_from(&root).unwrap();
        let temp = find_metric(&metrics, "node_thermal_zone_temp");
        assert_eq!(temp.samples.len(), 1);
    }
}
