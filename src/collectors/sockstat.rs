//! `/proc/net/sockstat` and `/proc/net/sockstat6` — per-protocol socket
//! counts.
//!
//! Emits `node_sockstat_<Proto>_<Field>` gauges, wire-compatible with
//! upstream `node_exporter`. The IPv4 file additionally carries a
//! `sockets: used <n>` header row which becomes the special-cased
//! `node_sockstat_sockets_used` metric. The IPv6 file uses protocol
//! prefixes that include the trailing `6` (e.g. `TCP6:`, `UDP6:`) and
//! the resulting metric names keep that `6` — i.e. `node_sockstat_TCP6_inuse`.
//!
//! `mem` is reported by the kernel in pages; we additionally emit
//! `<Proto>_mem_bytes = mem * pagesize` to match upstream.
//!
//! Reference: `node_exporter/collector/sockstat_linux.go` and
//! `prometheus/procfs/net_sockstat.go`.
//!
//! On a kernel with IPv6 disabled, `/proc/net/sockstat6` does not exist;
//! that case is tolerated (logged at debug, no error).

use std::fs;
use std::io;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct SockstatCollector;

impl Collector for SockstatCollector {
    fn name(&self) -> &'static str {
        "sockstat"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let v4_path = cfg.proc_path("/proc/net/sockstat");
        let v4_raw = fs::read_to_string(&v4_path)
            .with_context(|| format!("reading {}", v4_path.display()))?;

        let v6_path = cfg.proc_path("/proc/net/sockstat6");
        let v6_raw = match fs::read_to_string(&v6_path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(path = %v6_path.display(), "sockstat6 not present; skipping");
                None
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", v6_path.display()));
            }
        };

        let pagesize = pagesize_bytes();
        parse(&v4_raw, v6_raw.as_deref(), pagesize)
    }
}

/// Read the system page size via `sysconf(_SC_PAGESIZE)`. Pulled out so
/// tests can supply a fixed value.
fn pagesize_bytes() -> u64 {
    // SAFETY: `sysconf` is a thread-safe POSIX call with no preconditions.
    let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if raw <= 0 {
        // Falls back to the near-universal 4 KiB. We never expect to hit
        // this on Linux, but a zero/negative value would produce nonsense
        // `mem_bytes` samples.
        4096
    } else {
        // `c_long` -> u64: positive after the guard above, so the cast is
        // safe on all supported platforms (32- and 64-bit).
        #[allow(clippy::cast_sign_loss)]
        {
            raw as u64
        }
    }
}

fn parse(v4: &str, v6: Option<&str>, pagesize: u64) -> anyhow::Result<Vec<Metric>> {
    let mut out: Vec<Metric> = Vec::with_capacity(32);

    parse_one(v4, false, pagesize, &mut out).context("parsing /proc/net/sockstat")?;
    if let Some(raw) = v6 {
        parse_one(raw, true, pagesize, &mut out).context("parsing /proc/net/sockstat6")?;
    }

    if out.is_empty() {
        return Err(anyhow!("sockstat: no values parsed"));
    }
    Ok(out)
}

fn parse_one(raw: &str, is_ipv6: bool, pagesize: u64, out: &mut Vec<Metric>) -> anyhow::Result<()> {
    for (lineno, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }

        // Split off the protocol prefix (e.g. "TCP:") from the key/value
        // tail. Upstream requires `len(fields) < 3` to reject — i.e. there
        // must be at least one full key/value pair after the prefix.
        let mut fields = line.split_ascii_whitespace();
        let proto_raw = fields
            .next()
            .ok_or_else(|| anyhow!("sockstat: line {} empty", lineno + 1))?;
        let proto = proto_raw
            .strip_suffix(':')
            .ok_or_else(|| anyhow!("sockstat: line {} missing ':' on {proto_raw:?}", lineno + 1))?;

        let kvs: Vec<&str> = fields.collect();
        if kvs.is_empty() || kvs.len() % 2 != 0 {
            return Err(anyhow!(
                "sockstat: line {} has malformed key/value pairs: {kvs:?}",
                lineno + 1
            ));
        }

        if proto == "sockets" && !is_ipv6 {
            // Special case: emit node_sockstat_sockets_used directly. The
            // IPv6 file does not carry a `sockets:` row but defensively
            // ignore it if it ever does.
            let used = lookup_kv(&kvs, "used").ok_or_else(|| {
                anyhow!("sockstat: line {} sockets row missing `used`", lineno + 1)
            })?;
            let value: i64 = used
                .parse()
                .with_context(|| format!("sockstat: parsing sockets.used = {used:?}"))?;
            #[allow(clippy::cast_precision_loss)]
            let v = value as f64;
            out.push(
                Metric::new(
                    "node_sockstat_sockets_used",
                    "Number of IPv4 sockets in use.",
                    MetricType::Gauge,
                )
                .with_sample(Sample::new(v)),
            );
            continue;
        }

        let mut mem_pages: Option<i64> = None;
        for chunk in kvs.chunks_exact(2) {
            let key = chunk[0];
            let val: i64 = chunk[1].parse().with_context(|| {
                format!(
                    "sockstat: parsing {proto}.{key} = {value:?}",
                    value = chunk[1]
                )
            })?;

            if key == "mem" {
                mem_pages = Some(val);
            }

            let name = format!("node_sockstat_{proto}_{key}");
            let help = format!("Number of {proto} sockets in state {key}.");
            #[allow(clippy::cast_precision_loss)]
            let v = val as f64;
            out.push(Metric::new(name, help, MetricType::Gauge).with_sample(Sample::new(v)));
        }

        // Derived `mem_bytes = mem * pagesize`. Matches upstream's
        // synthetic pair so dashboards keying off `node_sockstat_TCP_mem_bytes`
        // continue to work.
        if let Some(pages) = mem_pages {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
            let bytes = (pages as f64) * (pagesize as f64);
            let name = format!("node_sockstat_{proto}_mem_bytes");
            let help = format!("Number of {proto} sockets in state mem_bytes.");
            out.push(Metric::new(name, help, MetricType::Gauge).with_sample(Sample::new(bytes)));
        }
    }
    Ok(())
}

fn lookup_kv<'a>(kvs: &'a [&'a str], key: &str) -> Option<&'a str> {
    kvs.chunks_exact(2)
        .find_map(|c| if c[0] == key { Some(c[1]) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

    const V4_FIXTURE: &str = "\
sockets: used 234
TCP: inuse 12 orphan 0 tw 5 alloc 30 mem 4
UDP: inuse 8 mem 1
UDPLITE: inuse 0
RAW: inuse 1
FRAG: inuse 0 memory 0
";

    const V6_FIXTURE: &str = "\
TCP6: inuse 17
UDP6: inuse 3
UDPLITE6: inuse 0
RAW6: inuse 0
FRAG6: inuse 0 memory 0
";

    /// Convenience: fixed page size so `mem_bytes` is predictable.
    const TEST_PAGESIZE: u64 = 4096;

    fn find<'a>(metrics: &'a [Metric], name: &str) -> &'a Metric {
        metrics.iter().find(|m| m.name == name).unwrap_or_else(|| {
            panic!(
                "metric {name} not found in {:?}",
                metrics.iter().map(|m| &m.name).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn parses_v4_and_v6() {
        let metrics = parse(V4_FIXTURE, Some(V6_FIXTURE), TEST_PAGESIZE).unwrap();

        // Special-cased sockets_used.
        let su = find(&metrics, "node_sockstat_sockets_used");
        assert_eq!(su.mtype, MetricType::Gauge);
        assert!((su.samples[0].value - 234.0).abs() < f64::EPSILON);

        // TCP fields.
        assert!(
            (find(&metrics, "node_sockstat_TCP_inuse").samples[0].value - 12.0).abs()
                < f64::EPSILON
        );
        assert!((find(&metrics, "node_sockstat_TCP_orphan").samples[0].value).abs() < f64::EPSILON);
        assert!(
            (find(&metrics, "node_sockstat_TCP_tw").samples[0].value - 5.0).abs() < f64::EPSILON
        );
        assert!(
            (find(&metrics, "node_sockstat_TCP_alloc").samples[0].value - 30.0).abs()
                < f64::EPSILON
        );
        assert!(
            (find(&metrics, "node_sockstat_TCP_mem").samples[0].value - 4.0).abs() < f64::EPSILON
        );

        // mem_bytes = mem * pagesize = 4 * 4096 = 16384.
        assert!(
            (find(&metrics, "node_sockstat_TCP_mem_bytes").samples[0].value - 16_384.0).abs()
                < f64::EPSILON
        );

        // UDP: inuse + mem + mem_bytes.
        assert!(
            (find(&metrics, "node_sockstat_UDP_inuse").samples[0].value - 8.0).abs() < f64::EPSILON
        );
        assert!(
            (find(&metrics, "node_sockstat_UDP_mem_bytes").samples[0].value - 4096.0).abs()
                < f64::EPSILON
        );

        // UDPLITE / RAW: just inuse.
        assert!(
            (find(&metrics, "node_sockstat_UDPLITE_inuse").samples[0].value).abs() < f64::EPSILON
        );
        assert!(
            (find(&metrics, "node_sockstat_RAW_inuse").samples[0].value - 1.0).abs() < f64::EPSILON
        );

        // FRAG: inuse + memory (note: "memory", not "mem" → no mem_bytes).
        assert!((find(&metrics, "node_sockstat_FRAG_inuse").samples[0].value).abs() < f64::EPSILON);
        assert!(
            (find(&metrics, "node_sockstat_FRAG_memory").samples[0].value).abs() < f64::EPSILON
        );
        assert!(
            metrics
                .iter()
                .all(|m| m.name != "node_sockstat_FRAG_mem_bytes"),
            "FRAG has `memory` not `mem`; mem_bytes must not appear"
        );

        // IPv6: protocol prefix retains the `6`.
        assert!(
            (find(&metrics, "node_sockstat_TCP6_inuse").samples[0].value - 17.0).abs()
                < f64::EPSILON
        );
        assert!(
            (find(&metrics, "node_sockstat_UDP6_inuse").samples[0].value - 3.0).abs()
                < f64::EPSILON
        );
        assert!(
            (find(&metrics, "node_sockstat_FRAG6_memory").samples[0].value).abs() < f64::EPSILON
        );

        // IPv6 must NOT emit a `sockets_used` metric — that is IPv4-only
        // and the sockstat6 file has no `sockets:` row.
        let used_count = metrics
            .iter()
            .filter(|m| m.name == "node_sockstat_sockets_used")
            .count();
        assert_eq!(used_count, 1, "sockets_used must be IPv4-only");
    }

    #[test]
    fn tolerates_missing_sockstat6() {
        let metrics = parse(V4_FIXTURE, None, TEST_PAGESIZE).unwrap();
        // Spot-check: still got the v4 metrics and no TCP6.
        assert!(metrics.iter().any(|m| m.name == "node_sockstat_TCP_inuse"));
        assert!(metrics.iter().all(|m| !m.name.contains("TCP6")));
    }

    /// `anyhow::Error::to_string()` only shows the outermost context.
    /// Use the alternate (`{:#}`) format so inner messages are visible
    /// to `contains()` checks.
    fn full_chain(err: &anyhow::Error) -> String {
        format!("{err:#}")
    }

    #[test]
    fn rejects_malformed_line_missing_colon() {
        // "TCP" without the trailing colon is not a valid prefix row.
        let err = parse("TCP inuse 1\n", None, TEST_PAGESIZE).unwrap_err();
        let msg = full_chain(&err);
        assert!(msg.contains("missing ':'"), "{msg}");
    }

    #[test]
    fn rejects_odd_field_count() {
        // Odd number of post-prefix tokens (`inuse` with no value).
        let err = parse("TCP: inuse\n", None, TEST_PAGESIZE).unwrap_err();
        let msg = full_chain(&err);
        assert!(msg.contains("malformed key/value pairs"), "{msg}");
    }

    #[test]
    fn rejects_non_integer_value() {
        let err = parse("TCP: inuse banana\n", None, TEST_PAGESIZE).unwrap_err();
        let msg = full_chain(&err);
        assert!(msg.contains("parsing"), "{msg}");
    }

    #[test]
    fn rejects_empty_input() {
        let err = parse("", None, TEST_PAGESIZE).unwrap_err();
        let msg = full_chain(&err);
        assert!(msg.contains("no values parsed"), "{msg}");
    }

    #[test]
    fn accepts_extra_fields_in_pairs() {
        // Real kernels may grow the field list; we accept any even number
        // of post-prefix tokens.
        let raw = "TCP: inuse 1 orphan 2 tw 3 alloc 4 mem 5\n";
        let metrics = parse(raw, None, TEST_PAGESIZE).unwrap();
        assert!(
            (find(&metrics, "node_sockstat_TCP_alloc").samples[0].value - 4.0).abs() < f64::EPSILON
        );
        // mem_bytes = 5 * 4096 = 20480
        assert!(
            (find(&metrics, "node_sockstat_TCP_mem_bytes").samples[0].value - 20_480.0).abs()
                < f64::EPSILON
        );
    }
}
