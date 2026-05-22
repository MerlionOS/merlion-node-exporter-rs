# merlion-node-exporter-rs

Rust reimplementation of the Prometheus
[`node_exporter`](https://github.com/prometheus/node_exporter) — hardware
and OS metrics exposed by \*NIX kernels, in a single statically-linked
binary.

This is the Rust sibling of a planned
[`merlion-node-exporter-cpp`](https://github.com/MerlionOS/merlion-node-exporter-cpp),
following the same dual-language pattern as
[`merlion-tsdb-rs`](https://github.com/MerlionOS/merlion-tsdb-rs) /
[`merlion-tsdb-cpp`](https://github.com/MerlionOS/merlion-tsdb-cpp). Both
exporters are designed to pair naturally with the Merlion TSDB, but are
fully wire-compatible with vanilla Prometheus: they speak the standard
text exposition format on `/metrics` so existing scrapes, dashboards,
and alerting rules work unchanged.

The installed binary is named `merlion-node-exporter` (no language
suffix) — operators choose whichever implementation they prefer at
install time, then run the same command.

> **Status — early alpha.** The HTTP server, registry, and the
> `loadavg` / `meminfo` / `uname` collectors are functional. The
> remaining ~12 Linux MVP collectors land in follow-up PRs (see
> [Roadmap](#roadmap)).

## Scope

This repository targets the **Linux MVP** — ~15 high-value collectors
that cover the metrics Prometheus dashboards and the typical Grafana
node-exporter dashboard actually graph. The full upstream
node_exporter spans ~100 collectors and five operating-system kernels;
that breadth is not a goal here.

| Platform | Status |
| --- | --- |
| Linux (x86_64, aarch64) | Primary target |
| macOS | Builds and runs `uname` collector; `/proc`-based collectors degrade gracefully (success=0) |
| BSD / Solaris / AIX | Out of scope |

## Quick start

```bash
cargo run --release -- --web.listen-address :9100
curl http://localhost:9100/metrics
```

The default listen address is `:9100` and the default telemetry path is
`/metrics` — both match upstream `node_exporter` so Prometheus
`scrape_configs` need no changes.

## CLI

```text
--web.listen-address <ADDR>   Default :9100 (env MNE_LISTEN_ADDRESS)
--web.telemetry-path <PATH>   Default /metrics (env MNE_TELEMETRY_PATH)
--path.procfs <DIR>           Default /proc (env MNE_PROCFS)
--path.sysfs <DIR>            Default /sys  (env MNE_SYSFS)
--path.rootfs <DIR>           Default /     (env MNE_ROOTFS)
--no-collector <NAME>         Disable a collector. Repeatable.
--collector.only <NAME>       Enable only the named collectors. Repeatable.
```

Logging is controlled by `RUST_LOG` (`tracing-subscriber` env-filter
format); the default level is `info`.

## Container usage

When running inside a container, bind-mount the host root and point the
exporter at it, exactly as you would with upstream:

```bash
docker run --rm --net=host --pid=host \
  -v /:/host:ro,rslave \
  ghcr.io/merlionos/merlion-node-exporter:latest \
  --path.rootfs=/host \
  --path.procfs=/host/proc \
  --path.sysfs=/host/sys
```

(Container images are not published yet — track issue #1.)

## Roadmap

Scaffold PR — this repo at the time of writing:

- [x] HTTP server (`axum`), `/metrics` endpoint, graceful shutdown
- [x] Collector trait + registry with per-collector success/duration metrics
- [x] Prometheus text-format encoder (0.0.4)
- [x] `loadavg`, `meminfo`, `uname`
- [x] CLI flags matching upstream node_exporter conventions

Linux MVP — remaining 12 collectors, one PR each:

- [ ] `cpu` — `/proc/stat` per-CPU jiffies
- [ ] `diskstats` — `/proc/diskstats`
- [ ] `netdev` — `/proc/net/dev`
- [x] `filesystem` — `getmntinfo` + `statvfs`
- [ ] `stat` — `/proc/stat` (boot time, intr, ctxt, processes)
- [x] `vmstat` — `/proc/vmstat`
- [x] `stat` — `/proc/stat` (boot time, intr, ctxt, processes)
- [ ] `vmstat` — `/proc/vmstat`
- [ ] `netstat` — `/proc/net/{netstat,snmp,snmp6}`
- [ ] `sockstat` — `/proc/net/sockstat{,6}`
- [ ] `pressure` — `/proc/pressure/{cpu,memory,io}`
- [ ] `hwmon` — `/sys/class/hwmon/`
- [ ] `thermal_zone` — `/sys/class/thermal/thermal_zone*`
- [ ] `time` — system clock + NTP sync state
- [ ] `textfile` — `*.prom` files from a configured directory

Past MVP:

- [ ] Container image + Homebrew formula
- [ ] eBPF-backed collectors (TCP retransmits, runqlat, …) — gated behind a feature flag
- [ ] OpenMetrics protobuf negotiation

## Design notes

- **Per-scrape collection.** Every `/metrics` request re-reads
  `/proc` / `/sys`. No caching or background refresh — the kernel
  already exposes the data at sub-microsecond cost.
- **No Prometheus client library.** The metric model is a flat
  `Vec<Metric>` and the text-format encoder is ~80 LOC. This matches
  node_exporter's per-scrape pattern more naturally than the typed
  pre-registration model that the `prometheus-client` crate is
  optimised for; we can revisit if histograms or OpenMetrics
  negotiation become MVP requirements.
- **Collectors degrade individually.** A failing collector emits
  `node_scrape_collector_success{collector="..."} 0` and a
  `node_scrape_collector_duration_seconds` sample — partial scrape
  output is still useful.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

This crate uses Rust 2024 edition (`rustc >= 1.85`).

## License

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). Metric
names and CLI flags follow upstream node_exporter for compatibility;
see [NOTICE](NOTICE) for attribution.
