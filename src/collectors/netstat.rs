//! `/proc/net/{netstat,snmp,snmp6}` — per-protocol network counters.
//!
//! Three sources, one collector. All fields are emitted as untyped (matching
//! upstream `node_exporter` — see `netstat_linux.go`), under metric names
//! shaped like `node_netstat_<Proto>_<Field>`:
//!
//! - `/proc/net/netstat` and `/proc/net/snmp` share a "header line + value
//!   line" alternating layout, prefixed with the protocol name and a colon.
//!   Each header/value pair contributes one sample.
//! - `/proc/net/snmp6` uses one `Field<whitespace>Value` per line; the field
//!   name already embeds the protocol (e.g. `Ip6InReceives`). Upstream splits
//!   on the first `6` so the metric becomes `node_netstat_Ip6_InReceives` —
//!   we match that byte-for-byte.
//!
//! `/proc/net/snmp6` is absent on IPv6-disabled hosts; that's tolerated.
//! Either `netstat` or `snmp` failing bubbles up as an error.
//!
//! # Divergence from upstream
//!
//! Upstream gates emission behind `--collector.netstat.fields` (a regex
//! default-set to a curated subset). MVP emits every field unfiltered;
//! the flag can land later without breaking compatibility — opt-out, not
//! opt-in, of the existing names.

use std::fs;
use std::io;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct NetstatCollector;

impl Collector for NetstatCollector {
    fn name(&self) -> &'static str {
        "netstat"
    }

    // The parallel `snmp_path` / `snmp6_path` (and the matching `_raw`
    // pair) are intentional — they mirror the three /proc files this
    // collector consumes. Renaming them past the lint would obscure that
    // structure.
    #[allow(clippy::similar_names)]
    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let netstat_path = cfg.proc_path("/proc/net/netstat");
        let snmp_path = cfg.proc_path("/proc/net/snmp");
        let snmp6_path = cfg.proc_path("/proc/net/snmp6");

        let netstat_raw = fs::read_to_string(&netstat_path)
            .with_context(|| format!("reading {}", netstat_path.display()))?;
        let snmp_raw = fs::read_to_string(&snmp_path)
            .with_context(|| format!("reading {}", snmp_path.display()))?;
        // IPv6-disabled hosts have no snmp6 file. Treat NotFound as empty,
        // but propagate any other I/O error.
        let snmp6_raw = match fs::read_to_string(&snmp6_path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("reading {}", snmp6_path.display()))
                );
            }
        };

        let mut out = parse_netstat(&netstat_raw).context("parsing /proc/net/netstat")?;
        out.extend(parse_netstat(&snmp_raw).context("parsing /proc/net/snmp")?);
        if let Some(raw) = snmp6_raw {
            out.extend(parse_snmp6(&raw).context("parsing /proc/net/snmp6")?);
        }
        Ok(out)
    }
}

/// Parse the alternating `Proto: H1 H2 ...` / `Proto: V1 V2 ...` layout
/// used by both `/proc/net/netstat` and `/proc/net/snmp`. Emits one metric
/// family per (protocol, field) pair.
fn parse_netstat(raw: &str) -> anyhow::Result<Vec<Metric>> {
    let mut out: Vec<Metric> = Vec::new();
    let mut lines = raw.lines();

    while let Some(header_line) = lines.next() {
        if header_line.is_empty() {
            continue;
        }
        let value_line = lines
            .next()
            .ok_or_else(|| anyhow!("header line {header_line:?} has no matching value line"))?;

        // Upstream splits on a single ASCII space; field names never contain
        // spaces so this matches.
        let header_parts: Vec<&str> = header_line.split(' ').collect();
        let value_parts: Vec<&str> = value_line.split(' ').collect();

        let proto_token = header_parts
            .first()
            .ok_or_else(|| anyhow!("empty header line"))?;
        let protocol = proto_token
            .strip_suffix(':')
            .ok_or_else(|| anyhow!("header token {proto_token:?} missing trailing ':'"))?;

        // The value line's first token must match (with the same `Proto:`
        // prefix) — upstream doesn't enforce this, but its `len` check on
        // the same arrays catches the same family of corruption.
        if header_parts.len() != value_parts.len() {
            return Err(anyhow!(
                "field count mismatch for protocol {protocol}: \
                 {} headers vs {} values",
                header_parts.len() - 1,
                value_parts.len() - 1,
            ));
        }

        for (field, value_str) in header_parts.iter().zip(value_parts.iter()).skip(1) {
            let value: f64 = value_str
                .parse()
                .with_context(|| format!("invalid value {value_str:?} for {protocol}_{field}"))?;
            let name = format!("node_netstat_{protocol}_{field}");
            let help = format!("Statistic {protocol}{field}.");
            out.push(Metric::new(name, help, MetricType::Untyped).with_sample(Sample::new(value)));
        }
    }

    Ok(out)
}

/// Parse `/proc/net/snmp6`, which uses one `Field   Value` pair per line.
/// The field name already embeds the protocol; upstream splits at the first
/// `6` (e.g. `Ip6InReceives` → protocol `Ip6`, name `InReceives`). Lines
/// whose first token contains no `6` are skipped, matching upstream.
fn parse_snmp6(raw: &str) -> anyhow::Result<Vec<Metric>> {
    let mut out: Vec<Metric> = Vec::new();

    for line in raw.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(field), Some(value_str)) = (fields.next(), fields.next()) else {
            continue;
        };

        let Some(six_idx) = field.find('6') else {
            continue;
        };
        let protocol = &field[..=six_idx];
        let name = &field[six_idx + 1..];
        if name.is_empty() {
            continue;
        }

        let value: f64 = value_str
            .parse()
            .with_context(|| format!("invalid value {value_str:?} for {protocol}_{name}"))?;
        let metric_name = format!("node_netstat_{protocol}_{name}");
        let help = format!("Statistic {protocol}{name}.");
        out.push(
            Metric::new(metric_name, help, MetricType::Untyped).with_sample(Sample::new(value)),
        );
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Representative subset of upstream's
    /// `node_exporter/collector/fixtures/proc/net/netstat`.
    const NETSTAT_FIXTURE: &str = "\
TcpExt: SyncookiesSent SyncookiesRecv TCPOFOQueue TCPRcvQDrop
TcpExt: 0 0 42 131
IpExt: InOctets OutOctets
IpExt: 6286396970 2786264347
";

    /// Subset of upstream's `/proc/net/snmp` fixture.
    const SNMP_FIXTURE: &str = "\
Ip: Forwarding DefaultTTL InReceives
Ip: 1 64 57740232
Tcp: ActiveOpens PassiveOpens InSegs OutSegs
Tcp: 3556 230 57252008 54915039
Udp: InDatagrams NoPorts
Udp: 88542 120
";

    /// Subset of upstream's `/proc/net/snmp6` fixture, covering each
    /// protocol prefix that the snmp6 parser is expected to handle.
    const SNMP6_FIXTURE: &str = "\
Ip6InReceives                   \t7
Ip6InHdrErrors                  \t0
Ip6OutOctets                    \t536
Icmp6InMsgs                     \t0
Icmp6OutMsgs                    \t8
Udp6InDatagrams                 \t0
Udp6RcvbufErrors                \t9
UdpLite6InDatagrams             \t0
";

    fn sample_value(metrics: &[Metric], name: &str) -> f64 {
        metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("metric {name} not found"))
            .samples[0]
            .value
    }

    #[test]
    fn parses_netstat_fixture() {
        let metrics = parse_netstat(NETSTAT_FIXTURE).unwrap();
        // 4 TcpExt fields + 2 IpExt fields.
        assert_eq!(metrics.len(), 6);

        assert!(
            (sample_value(&metrics, "node_netstat_TcpExt_SyncookiesSent")).abs() < f64::EPSILON
        );
        assert!(
            (sample_value(&metrics, "node_netstat_TcpExt_TCPRcvQDrop") - 131.0).abs()
                < f64::EPSILON
        );
        assert!(
            (sample_value(&metrics, "node_netstat_IpExt_InOctets") - 6_286_396_970.0).abs() < 1.0
        );

        // Upstream emits Untyped.
        for m in &metrics {
            assert_eq!(m.mtype, MetricType::Untyped, "{} should be untyped", m.name);
        }
    }

    #[test]
    fn parses_snmp_fixture() {
        let metrics = parse_netstat(SNMP_FIXTURE).unwrap();
        // 3 Ip + 4 Tcp + 2 Udp = 9 fields.
        assert_eq!(metrics.len(), 9);
        assert!((sample_value(&metrics, "node_netstat_Ip_Forwarding") - 1.0).abs() < f64::EPSILON);
        assert!((sample_value(&metrics, "node_netstat_Tcp_InSegs") - 57_252_008.0).abs() < 1.0);
    }

    #[test]
    fn parses_snmp6_fixture() {
        let metrics = parse_snmp6(SNMP6_FIXTURE).unwrap();
        assert_eq!(metrics.len(), 8);
        assert!((sample_value(&metrics, "node_netstat_Ip6_InReceives") - 7.0).abs() < f64::EPSILON);
        assert!((sample_value(&metrics, "node_netstat_Icmp6_OutMsgs") - 8.0).abs() < f64::EPSILON);
        assert!(
            (sample_value(&metrics, "node_netstat_Udp6_RcvbufErrors") - 9.0).abs() < f64::EPSILON
        );
        assert!((sample_value(&metrics, "node_netstat_UdpLite6_InDatagrams")).abs() < f64::EPSILON);
        for m in &metrics {
            assert_eq!(m.mtype, MetricType::Untyped);
        }
    }

    #[test]
    fn snmp6_skips_lines_without_six() {
        // Upstream skips lines whose first token contains no '6'. We do too.
        let raw = "TcpExtSomething 42\nIp6InReceives 7\n";
        let metrics = parse_snmp6(raw).unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "node_netstat_Ip6_InReceives");
    }

    #[test]
    fn snmp6_skips_short_lines() {
        let raw = "OnlyOneToken\nIp6InReceives 7\n";
        let metrics = parse_snmp6(raw).unwrap();
        assert_eq!(metrics.len(), 1);
    }

    #[test]
    fn rejects_orphan_header_line() {
        // Header with no trailing value line.
        let raw = "Tcp: A B C\n";
        let err = parse_netstat(raw).unwrap_err().to_string();
        assert!(err.contains("no matching value line"), "{err}");
    }

    #[test]
    fn rejects_header_value_count_mismatch() {
        let raw = "Tcp: A B C\nTcp: 1 2\n";
        let err = parse_netstat(raw).unwrap_err().to_string();
        assert!(err.contains("field count mismatch"), "{err}");
    }

    #[test]
    fn rejects_header_missing_colon() {
        let raw = "Tcp A B\nTcp 1 2\n";
        let err = parse_netstat(raw).unwrap_err().to_string();
        assert!(err.contains("trailing ':'"), "{err}");
    }

    #[test]
    fn rejects_non_numeric_value() {
        let raw = "Tcp: A\nTcp: banana\n";
        let err = parse_netstat(raw).unwrap_err().to_string();
        assert!(err.contains("invalid value"), "{err}");
    }

    #[test]
    fn rejects_non_numeric_snmp6_value() {
        let err = parse_snmp6("Ip6InReceives banana\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid value"), "{err}");
    }
}
