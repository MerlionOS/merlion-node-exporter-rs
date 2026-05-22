//! `/proc/net/dev` — per-interface network counters.
//!
//! Emits sixteen counter families, byte-compatible with upstream
//! `node_exporter` running with its default (non-detailed) metric set:
//!
//! - Receive side: `node_network_receive_{bytes,packets,errs,drop,fifo,
//!   frame,compressed,multicast}_total`.
//! - Transmit side: `node_network_transmit_{bytes,packets,errs,drop,fifo,
//!   colls,carrier,compressed}_total`.
//!
//! Each sample carries a single `device="<ifname>"` label.
//!
//! Reference: `node_exporter/collector/netdev_linux.go` (`procNetDevStats`)
//! and `netdev_common.go` (`legacy()` post-processing — which renames
//! `receive_errors`/`receive_dropped`/`receive_fifo_errors` to the short
//! `_errs`/`_drop`/`_fifo` forms when `--collector.netdev.enable-detailed-metrics`
//! is unset, the default).

use std::fs;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct NetdevCollector;

impl Collector for NetdevCollector {
    fn name(&self) -> &'static str {
        "netdev"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let path = cfg.proc_path("/proc/net/dev");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        parse(&raw)
    }
}

/// Field order in `/proc/net/dev`, post-colon. The kernel writes 16
/// counters per interface — 8 RX followed by 8 TX. See
/// `net/core/net-procfs.c:dev_seq_printf_stats` in the Linux source.
///
/// Each entry is the metric short name (the bit after `node_network_`
/// and before `_total`). Names match upstream's "legacy" (default)
/// labelling — `errs`/`drop`/`fifo`, not `errors`/`dropped`/`fifo_errors`.
const FIELD_NAMES: [&str; 16] = [
    "receive_bytes",
    "receive_packets",
    "receive_errs",
    "receive_drop",
    "receive_fifo",
    "receive_frame",
    "receive_compressed",
    "receive_multicast",
    "transmit_bytes",
    "transmit_packets",
    "transmit_errs",
    "transmit_drop",
    "transmit_fifo",
    "transmit_colls",
    "transmit_carrier",
    "transmit_compressed",
];

// u64 -> f64 for counter export is the standard Prometheus convention;
// precision loss past 2^53 is documented in upstream and not worth
// guarding against here.
#[allow(clippy::cast_precision_loss)]
fn parse(raw: &str) -> anyhow::Result<Vec<Metric>> {
    let mut metrics: Vec<Metric> = FIELD_NAMES
        .iter()
        .map(|key| {
            Metric::new(
                format!("node_network_{key}_total"),
                format!("Network device statistic {key}."),
                MetricType::Counter,
            )
        })
        .collect();

    let mut saw_any = false;
    for (lineno, line) in raw.lines().enumerate() {
        // Skip the two header rows. The kernel writes them as the first
        // two lines of the file; everything after is one row per
        // interface.
        if lineno < 2 {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }

        // Use the *last* ':' as the separator: interface names can
        // themselves contain a colon (e.g. VLAN aliases like `eth0:0`),
        // and the kernel right-pads the name into a fixed-width column
        // before the colon. Matches the procfs library's behaviour.
        let idx = line
            .rfind(':')
            .ok_or_else(|| anyhow!("netdev: line {} missing ':'", lineno + 1))?;
        let name = line[..idx].trim();
        if name.is_empty() {
            return Err(anyhow!(
                "netdev: line {} has empty interface name",
                lineno + 1
            ));
        }

        let mut fields = line[idx + 1..].split_ascii_whitespace();
        for (i, key) in FIELD_NAMES.iter().enumerate() {
            let tok = fields.next().ok_or_else(|| {
                anyhow!(
                    "netdev: {name}: missing field {i} ({key}) on line {}",
                    lineno + 1
                )
            })?;
            let v: u64 = tok
                .parse()
                .with_context(|| format!("netdev: {name}: parsing {key}"))?;
            metrics[i].push(Sample::new(v as f64).with_label("device", name));
        }
        saw_any = true;
    }

    if !saw_any {
        return Err(anyhow!("netdev: no interfaces parsed from /proc/net/dev"));
    }
    Ok(metrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real-looking /proc/net/dev fixture — two header rows then `lo`
    // and `eth0`. Values are arbitrary but each interface uses
    // distinct numbers so per-field assertions are unambiguous.
    const FIXTURE: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000      10    0    0    0    0       0          0       1000      10    0    0    0    0       0          0
  eth0: 2000000  2000   1    2    3    4       5          6       3000000  3000   7    8    9   10      11         12
";

    #[test]
    fn parses_happy_path() {
        let metrics = parse(FIXTURE).unwrap();
        assert_eq!(metrics.len(), 16, "16 metric families (8 RX + 8 TX)");

        // Spot-check names and types.
        assert_eq!(metrics[0].name, "node_network_receive_bytes_total");
        assert_eq!(metrics[0].mtype, MetricType::Counter);
        assert_eq!(metrics[15].name, "node_network_transmit_compressed_total");

        // Every family has exactly one sample per interface (2).
        for m in &metrics {
            assert_eq!(m.samples.len(), 2, "{} should have 2 samples", m.name);
            assert_eq!(m.samples[0].labels[0].0, "device");
            assert_eq!(m.samples[0].labels[0].1, "lo");
            assert_eq!(m.samples[1].labels[0].1, "eth0");
        }

        // RX bytes for eth0 = 2_000_000.
        let rx_bytes = &metrics[0];
        assert!((rx_bytes.samples[1].value - 2_000_000.0).abs() < f64::EPSILON);

        // TX packets for eth0 = 3000. Field index 9.
        let tx_packets = &metrics[9];
        assert_eq!(tx_packets.name, "node_network_transmit_packets_total");
        assert!((tx_packets.samples[1].value - 3000.0).abs() < f64::EPSILON);

        // transmit_colls for eth0 = 10. Field index 13.
        let colls = &metrics[13];
        assert_eq!(colls.name, "node_network_transmit_colls_total");
        assert!((colls.samples[1].value - 10.0).abs() < f64::EPSILON);

        // transmit_carrier for eth0 = 11. Field index 14.
        let carrier = &metrics[14];
        assert_eq!(carrier.name, "node_network_transmit_carrier_total");
        assert!((carrier.samples[1].value - 11.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_interface_name_with_colon() {
        // VLAN-style aliases like `eth0:0` contain a colon in the name;
        // we must use the *last* ':' as the field separator.
        let raw = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
 eth0:0: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16
";
        let metrics = parse(raw).unwrap();
        assert_eq!(metrics[0].samples.len(), 1);
        assert_eq!(metrics[0].samples[0].labels[0].1, "eth0:0");
        // First field = receive_bytes = 1.
        assert!((metrics[0].samples[0].value - 1.0).abs() < f64::EPSILON);
        // Last field = transmit_compressed = 16.
        assert!((metrics[15].samples[0].value - 16.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_empty_file() {
        let err = parse("").unwrap_err().to_string();
        assert!(err.contains("no interfaces parsed"), "{err}");
    }

    #[test]
    fn rejects_headers_only() {
        // Two header rows and nothing else — no interface data.
        let raw = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("no interfaces parsed"), "{err}");
    }

    #[test]
    fn rejects_line_missing_colon() {
        let raw = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
lo 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16
";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("missing ':'"), "{err}");
    }

    #[test]
    fn rejects_truncated_counter_row() {
        // Only 5 counters supplied; needs 16.
        let raw = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
 lo: 1 2 3 4 5
";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("missing field"), "{err}");
    }

    #[test]
    fn rejects_non_integer_counter() {
        let raw = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
 lo: 1 banana 3 4 5 6 7 8 9 10 11 12 13 14 15 16
";
        let err = parse(raw).unwrap_err().to_string();
        assert!(err.contains("parsing"), "{err}");
    }

    #[test]
    fn skips_blank_lines_between_interfaces() {
        let raw = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16

  eth0: 16 15 14 13 12 11 10 9 8 7 6 5 4 3 2 1
";
        let metrics = parse(raw).unwrap();
        assert_eq!(metrics[0].samples.len(), 2);
        assert_eq!(metrics[0].samples[0].labels[0].1, "lo");
        assert_eq!(metrics[0].samples[1].labels[0].1, "eth0");
    }
}
