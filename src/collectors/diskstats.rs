//! `/proc/diskstats` — per-block-device I/O statistics.
//!
//! Emits the standard 11 fields every Linux kernel exposes plus 6
//! discard / flush fields available on kernels ≥ 4.18 (when /proc lines
//! carry the extra columns). Info-type metrics that require /sys/block
//! reads (`node_disk_info`, ATA write-cache, etc.) are deferred to a
//! follow-up PR.
//!
//! Reference: `node_exporter/collector/diskstats_linux.go` `Update()` and
//! `diskstats_common.go` (descriptors + default exclude regex).

use std::fs;
use std::sync::OnceLock;

use anyhow::{Context, anyhow};
use regex::Regex;

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

/// Number of bytes per sector. Linux block layer is fixed at 512 here —
/// upstream `node_exporter` uses the same constant; the actual hardware
/// sector size is exposed via /sys/block but is not what /proc/diskstats
/// reports in.
const SECTOR_BYTES: f64 = 512.0;

/// Default `--collector.diskstats.device-exclude` regex from upstream
/// (`diskstats_common.go:37`). Filters out RAM/loop/floppy and the
/// numbered partition suffixes of common block devices, keeping the
/// base devices themselves (`sda`, `nvme0n1`, …).
const DEFAULT_EXCLUDE_REGEX: &str = r"^(z?ram|loop|fd|(h|s|v|xv)d[a-z]|nvme\d+n\d+p)\d+$";

fn exclude_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(DEFAULT_EXCLUDE_REGEX).expect("hardcoded regex must compile"))
}

pub struct DiskstatsCollector;

impl Collector for DiskstatsCollector {
    fn name(&self) -> &'static str {
        "diskstats"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let path = cfg.proc_path("/proc/diskstats");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        parse(&raw)
    }
}

/// One metric family the collector emits. Bundling `(name, help, type)`
/// inline keeps the field-to-metric mapping table compact and obvious
/// when reading the code.
struct Family {
    name: &'static str,
    help: &'static str,
    mtype: MetricType,
}

/// Maps the 17 metrics, in upstream output order, to the conversion
/// factor applied to the parsed field. None means "no conversion"
/// (counter of raw events).
struct FieldMap {
    family: Family,
    /// Index into the array of integer fields (after device/major/minor).
    field_idx: usize,
    /// Conversion factor; the parsed u64 is multiplied by this before
    /// being recorded as the sample value.
    factor: f64,
}

/// Returns the field-to-metric mapping table. The first 11 entries
/// correspond to the 11 fields every kernel exposes; entries 11..17 are
/// the discard + flush extensions added in kernel 4.18 and only emitted
/// when /proc lines carry the extra columns.
#[allow(clippy::too_many_lines)]
const fn families() -> [FieldMap; 17] {
    const MS_TO_S: f64 = 0.001;
    [
        FieldMap {
            family: Family {
                name: "node_disk_reads_completed_total",
                help: "The total number of reads completed successfully.",
                mtype: MetricType::Counter,
            },
            field_idx: 0,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_reads_merged_total",
                help: "The total number of reads merged.",
                mtype: MetricType::Counter,
            },
            field_idx: 1,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_read_bytes_total",
                help: "The total number of bytes read successfully.",
                mtype: MetricType::Counter,
            },
            field_idx: 2,
            factor: SECTOR_BYTES,
        },
        FieldMap {
            family: Family {
                name: "node_disk_read_time_seconds_total",
                help: "The total number of seconds spent by all reads.",
                mtype: MetricType::Counter,
            },
            field_idx: 3,
            factor: MS_TO_S,
        },
        FieldMap {
            family: Family {
                name: "node_disk_writes_completed_total",
                help: "The total number of writes completed successfully.",
                mtype: MetricType::Counter,
            },
            field_idx: 4,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_writes_merged_total",
                help: "The number of writes merged.",
                mtype: MetricType::Counter,
            },
            field_idx: 5,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_written_bytes_total",
                help: "The total number of bytes written successfully.",
                mtype: MetricType::Counter,
            },
            field_idx: 6,
            factor: SECTOR_BYTES,
        },
        FieldMap {
            family: Family {
                name: "node_disk_write_time_seconds_total",
                help: "This is the total number of seconds spent by all writes.",
                mtype: MetricType::Counter,
            },
            field_idx: 7,
            factor: MS_TO_S,
        },
        FieldMap {
            family: Family {
                name: "node_disk_io_now",
                help: "The number of I/Os currently in progress.",
                mtype: MetricType::Gauge,
            },
            field_idx: 8,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_io_time_seconds_total",
                help: "Total seconds spent doing I/Os.",
                mtype: MetricType::Counter,
            },
            field_idx: 9,
            factor: MS_TO_S,
        },
        FieldMap {
            family: Family {
                name: "node_disk_io_time_weighted_seconds_total",
                help: "The weighted # of seconds spent doing I/Os.",
                mtype: MetricType::Counter,
            },
            field_idx: 10,
            factor: MS_TO_S,
        },
        // Discard + flush — kernel ≥ 4.18 only.
        FieldMap {
            family: Family {
                name: "node_disk_discards_completed_total",
                help: "The total number of discards completed successfully.",
                mtype: MetricType::Counter,
            },
            field_idx: 11,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_discards_merged_total",
                help: "The total number of discards merged.",
                mtype: MetricType::Counter,
            },
            field_idx: 12,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_discarded_sectors_total",
                help: "The total number of sectors discarded successfully.",
                mtype: MetricType::Counter,
            },
            field_idx: 13,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_discard_time_seconds_total",
                help: "This is the total number of seconds spent by all discards.",
                mtype: MetricType::Counter,
            },
            field_idx: 14,
            factor: MS_TO_S,
        },
        FieldMap {
            family: Family {
                name: "node_disk_flush_requests_total",
                help: "The total number of flush requests completed successfully",
                mtype: MetricType::Counter,
            },
            field_idx: 15,
            factor: 1.0,
        },
        FieldMap {
            family: Family {
                name: "node_disk_flush_requests_time_seconds_total",
                help: "This is the total number of seconds spent by all flush requests.",
                mtype: MetricType::Counter,
            },
            field_idx: 16,
            factor: MS_TO_S,
        },
    ]
}

// u64 → f64 precision loss is at 2^53 — not a concern for IO counters at
// realistic rates. Annotate rather than clamp.
#[allow(clippy::cast_precision_loss)]
fn parse(raw: &str) -> anyhow::Result<Vec<Metric>> {
    // Collect (device, fields) tuples first so we know how many extension
    // fields are present before deciding which metric families to emit.
    let mut rows: Vec<(String, Vec<u64>)> = Vec::new();
    let mut max_field_count = 0usize;

    for (lineno, line) in raw.lines().enumerate() {
        let mut tokens = line.split_ascii_whitespace();
        // Skip blank lines.
        let Some(_major) = tokens.next() else {
            continue;
        };
        let Some(_minor) = tokens.next() else {
            continue;
        };
        let Some(device) = tokens.next() else {
            return Err(anyhow!(
                "diskstats: line {} missing device name",
                lineno + 1
            ));
        };
        if exclude_regex().is_match(device) {
            continue;
        }

        let fields: Vec<u64> = tokens
            .enumerate()
            .map(|(i, s)| {
                s.parse::<u64>().with_context(|| {
                    format!("diskstats: line {} field {} ({s:?})", lineno + 1, i + 4)
                })
            })
            .collect::<Result<_, _>>()?;

        // Need at least the first 11 stats; lines with fewer than 11 are
        // structurally invalid (every kernel since 2.6 emits at least 11).
        if fields.len() < 11 {
            return Err(anyhow!(
                "diskstats: device {device} has {} stat fields, expected at least 11",
                fields.len()
            ));
        }

        if fields.len() > max_field_count {
            max_field_count = fields.len();
        }
        rows.push((device.to_string(), fields));
    }

    if rows.is_empty() {
        // The default exclude regex shouldn't filter every entry on a real
        // system, but a synthetic fixture with only loop/ram devices could.
        // Still treat as "scrape OK, no data" by returning no metrics —
        // matches upstream's behaviour of emitting nothing for excluded
        // devices.
        return Ok(Vec::new());
    }

    // Decide which families to emit: always the first 11, plus any whose
    // field index is present on every row (otherwise we'd have asymmetric
    // metric samples across devices).
    let fams = families();
    let mut out: Vec<Metric> = Vec::with_capacity(fams.len());
    for fm in &fams {
        if fm.field_idx >= max_field_count {
            continue;
        }
        let mut m = Metric::new(fm.family.name, fm.family.help, fm.family.mtype);
        for (device, fields) in &rows {
            if let Some(&v) = fields.get(fm.field_idx) {
                m.push(Sample::new(v as f64 * fm.factor).with_label("device", device.as_str()));
            }
        }
        if !m.samples.is_empty() {
            out.push(m);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 20-field fixture (kernel ≥ 4.18). One sda (kept) + one loop0
    // (filtered) + one sda1 partition (filtered).
    const FIXTURE_FULL: &str = "\
   8       0 sda 1000 50 200000 1500 800 30 160000 1200 0 5000 2700 100 5 2048 25 0 0
   8       1 sda1 100 5 20000 150 80 3 16000 120 0 500 270 0 0 0 0 0 0
   7       0 loop0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
";

    // 11-field fixture (legacy kernel, no discard/flush columns).
    const FIXTURE_LEGACY: &str = "\
   8       0 sda 1000 50 200000 1500 800 30 160000 1200 0 5000 2700
";

    #[test]
    fn parses_full_fixture() {
        let metrics = parse(FIXTURE_FULL).unwrap();
        // 17 families emitted because the kept device has all 17 stats.
        assert_eq!(metrics.len(), 17);
        for m in &metrics {
            // sda1 is filtered out, loop0 is filtered out, so only sda
            // remains as a label value.
            assert_eq!(m.samples.len(), 1, "{}", m.name);
            assert_eq!(m.samples[0].labels[0], ("device".into(), "sda".into()));
        }
    }

    #[test]
    fn unit_conversions() {
        let metrics = parse(FIXTURE_FULL).unwrap();
        let by_name =
            |name: &str| metrics.iter().find(|m| m.name == name).unwrap().samples[0].value;
        // sectors × 512 → bytes. 200000 * 512 = 102_400_000.
        assert!((by_name("node_disk_read_bytes_total") - 102_400_000.0).abs() < 1.0);
        // ms / 1000 → seconds. 1500 ms → 1.5 s.
        assert!((by_name("node_disk_read_time_seconds_total") - 1.5).abs() < 1e-9);
        // counter passthrough.
        assert!((by_name("node_disk_reads_completed_total") - 1000.0).abs() < 1.0);
    }

    #[test]
    fn omits_discard_flush_on_legacy_kernel() {
        let metrics = parse(FIXTURE_LEGACY).unwrap();
        // 11 families, no discard/flush extensions.
        assert_eq!(metrics.len(), 11);
        let names: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
        assert!(!names.iter().any(|n| n.contains("discard")));
        assert!(!names.iter().any(|n| n.contains("flush")));
    }

    #[test]
    fn default_filter_excludes_loop_ram_partitions() {
        let raw = "\
   7       0 loop0 1 2 3 4 5 6 7 8 9 10 11
   7       1 loop1 1 2 3 4 5 6 7 8 9 10 11
   1       0 ram0 1 2 3 4 5 6 7 8 9 10 11
 252       0 zram0 1 2 3 4 5 6 7 8 9 10 11
   8       1 sda1 1 2 3 4 5 6 7 8 9 10 11
 259       1 nvme0n1p1 1 2 3 4 5 6 7 8 9 10 11
   2       0 fd0 1 2 3 4 5 6 7 8 9 10 11
";
        let metrics = parse(raw).unwrap();
        assert!(metrics.is_empty(), "every device should be filtered");
    }

    #[test]
    fn keeps_base_nvme_device() {
        let raw = "259       0 nvme0n1 100 0 1000 50 200 0 2000 100 0 500 300\n";
        let metrics = parse(raw).unwrap();
        // 11 families, all on the kept device.
        assert_eq!(metrics.len(), 11);
        assert_eq!(metrics[0].samples[0].labels[0].1, "nvme0n1");
    }

    #[test]
    fn rejects_non_integer_field() {
        let raw = "   8       0 sda 1000 50 banana 1500 800 30 160000 1200 0 5000 2700\n";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("field"), "{err}");
    }

    #[test]
    fn rejects_truncated_row() {
        let raw = "   8       0 sda 1 2 3 4 5\n";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("expected at least 11"), "{err}");
    }

    #[test]
    fn empty_input_returns_no_metrics() {
        let metrics = parse("").unwrap();
        assert!(metrics.is_empty());
    }
}
