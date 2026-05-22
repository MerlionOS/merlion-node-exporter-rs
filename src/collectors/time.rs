//! System clock + NTP/PLL state.
//!
//! Cross-platform metrics:
//!
//! - `node_time_seconds` (gauge) — wall-clock seconds since the Unix
//!   epoch, read via `clock_gettime(CLOCK_REALTIME)`. Matches upstream
//!   `time.Now().UnixNano() / 1e9` semantics byte-for-byte.
//! - `node_time_zone_offset_seconds{time_zone}` (gauge) — offset of
//!   local time from UTC. The zone label is the abbreviation from
//!   `localtime_r`'s `tm_zone` (e.g. `PDT`, `UTC`, `JST`); the value is
//!   `tm_gmtoff`. On platforms without `tm_gmtoff` the metric is
//!   omitted; in practice every Unix kernel that `libc` targets has it.
//!
//! Linux-only metrics (gated on `#[cfg(target_os = "linux")]`):
//!
//! - The full `node_timex_*` family — phase-locked-loop state, pulse-
//!   per-second statistics, and sync status as reported by
//!   `adjtimex(2)`. Mirrors upstream `collector/timex.go`.
//!
//! Reference: `node_exporter/collector/time.go` and `timex.go`.

use std::ffi::CStr;
use std::mem::MaybeUninit;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct TimeCollector;

impl Collector for TimeCollector {
    fn name(&self) -> &'static str {
        "time"
    }

    fn collect(&self, _cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let mut out = Vec::with_capacity(2 + TIMEX_METRIC_COUNT);

        let now = read_realtime_seconds().context("clock_gettime(CLOCK_REALTIME) failed")?;
        out.push(
            Metric::new(
                "node_time_seconds",
                "System time in seconds since epoch (1970).",
                MetricType::Gauge,
            )
            .with_sample(Sample::new(now)),
        );

        if let Some((zone, offset)) = read_local_zone() {
            // i64 → f64: tm_gmtoff is a c_long ranging within ±50 000;
            // any IEEE-754 double can represent it exactly.
            #[allow(clippy::cast_precision_loss)]
            let offset_f = offset as f64;
            out.push(
                Metric::new(
                    "node_time_zone_offset_seconds",
                    "System time zone offset in seconds.",
                    MetricType::Gauge,
                )
                .with_sample(Sample::new(offset_f).with_label("time_zone", zone)),
            );
        }

        #[cfg(target_os = "linux")]
        {
            match linux::read_timex() {
                Ok(timex) => out.extend(linux::compute_timex_metrics(&timex)),
                Err(e) => {
                    // adjtimex requiring CAP_SYS_TIME / root in some
                    // environments is an expected partial failure. Mirror
                    // upstream's debug-log + skip behaviour: report via
                    // the collector's success metric (by returning Err)
                    // only if we *really* couldn't talk to the kernel.
                    tracing::debug!(error = %e, "skipping timex metrics");
                }
            }
        }

        Ok(out)
    }
}

/// Number of metric families the Linux timex block contributes when
/// available. Used to pre-size the output Vec. (17 families: see
/// `linux::compute_timex_metrics`.)
#[cfg(target_os = "linux")]
const TIMEX_METRIC_COUNT: usize = 17;
#[cfg(not(target_os = "linux"))]
const TIMEX_METRIC_COUNT: usize = 0;

fn read_realtime_seconds() -> anyhow::Result<f64> {
    // SAFETY: `clock_gettime` writes a `timespec` and returns 0 on
    // success; we never read uninitialised memory on the error path.
    let mut ts = MaybeUninit::<libc::timespec>::uninit();
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, ts.as_mut_ptr()) };
    if rc != 0 {
        return Err(anyhow!(std::io::Error::last_os_error()));
    }
    let ts = unsafe { ts.assume_init() };
    // tv_sec is i64-ish (time_t) and tv_nsec is c_long in [0, 1e9).
    // Both convert losslessly to f64 for any realistic wall-clock value
    // (2^53 seconds ≈ 285 million years).
    #[allow(clippy::cast_precision_loss)]
    let secs = ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9;
    Ok(secs)
}

/// Read the local time zone abbreviation and UTC offset (in seconds).
///
/// Returns `None` if `localtime_r` fails or the platform reports an empty
/// zone abbreviation. Errors here are deliberately non-fatal: the system
/// clock metric should still be emitted even on the most exotic libc.
fn read_local_zone() -> Option<(String, i64)> {
    // `time(NULL)` is the standard way to obtain the current epoch time
    // for feeding into `localtime_r`. We have it already in
    // `read_realtime_seconds`, but `clock_gettime`/`time` results can be
    // re-obtained cheaply and keeping the two reads decoupled lets the
    // zone metric short-circuit independently.
    let mut now: libc::time_t = 0;
    // SAFETY: `time` writes to the supplied location if non-null, and
    // returns the same value. Both are safe to read.
    let rc = unsafe { libc::time(&raw mut now) };
    if rc == -1 {
        return None;
    }

    let mut tm = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `localtime_r(time, tm)` writes a fully-initialised tm on
    // success and returns the same pointer; on failure it returns NULL
    // and we must not read tm.
    let ret = unsafe { libc::localtime_r(&raw const now, tm.as_mut_ptr()) };
    if ret.is_null() {
        return None;
    }
    let tm = unsafe { tm.assume_init() };

    // tm_gmtoff is c_long — i64 on 64-bit targets (our supported
    // platforms) and i32 on 32-bit. Use `as i64` for a uniform widening
    // that compiles cleanly on either. Real-world offsets are ±14h,
    // well inside any integer width.
    #[allow(clippy::unnecessary_cast)] // c_long aliases differ across targets
    let offset = tm.tm_gmtoff as i64;

    let zone_ptr = tm.tm_zone;
    if zone_ptr.is_null() {
        // POSIX permits NULL here; fall back to a generic label rather
        // than dropping the metric.
        return Some((String::from("(unknown)"), offset));
    }
    // SAFETY: `tm.tm_zone` points into a static, NUL-terminated string
    // owned by libc (the tzdata-derived zone table). Lifetime exceeds
    // this function and the bytes are immutable.
    let zone = unsafe { CStr::from_ptr(zone_ptr) }
        .to_string_lossy()
        .into_owned();
    Some((zone, offset))
}

#[cfg(target_os = "linux")]
mod linux {
    //! adjtimex(2) — phase-locked loop and PPS state.
    //!
    //! All metric names, divisors, and the `STA_NANO` unit switch mirror
    //! `node_exporter/collector/timex.go`. Counter-typed metric values
    //! are signed in the kernel struct but always non-negative in
    //! practice (they're monotonically-incrementing event counters); we
    //! widen to f64 like upstream rather than rejecting negatives.

    use std::mem::MaybeUninit;

    use anyhow::anyhow;

    use crate::metric::{Metric, MetricType, Sample};

    /// `1e6 * 65536` — the kernel scales PPM frequency values by 2^16.
    /// See NOTES in `adjtimex(2)`.
    const PPM16_FRAC: f64 = 1_000_000.0 * 65536.0;
    const NANO: f64 = 1_000_000_000.0;
    const MICRO: f64 = 1_000_000.0;

    /// Call `adjtimex(buf)` in read-only mode (modes = 0). Returns the
    /// populated `timex` on success.
    pub(super) fn read_timex() -> anyhow::Result<libc::timex> {
        // SAFETY: `libc::timex` is plain-old-data and is required to be
        // zero-initialised for a read-only adjtimex query. We never read
        // it on the error path.
        let mut buf = MaybeUninit::<libc::timex>::zeroed();
        let rc = unsafe { libc::adjtimex(buf.as_mut_ptr()) };
        if rc == -1 {
            return Err(anyhow!(std::io::Error::last_os_error()));
        }
        // adjtimex returns the current time state (TIME_OK=0 … TIME_ERROR=5)
        // on success and -1 on failure. TIME_ERROR is still a successful
        // syscall — it just means the clock isn't synchronised — so we
        // accept any non-(-1) return.
        Ok(unsafe { buf.assume_init() })
    }

    /// Convert a populated `libc::timex` into the 17 metric families that
    /// upstream `node_exporter` emits. Pure function — easy to unit-test
    /// against a hand-constructed struct.
    //
    // Each kernel field becomes its own emitter line, so the function is a
    // wide-but-shallow list rather than something a helper extraction would
    // actually simplify. Splitting would just trade one length lint for
    // navigation overhead.
    #[allow(clippy::too_many_lines)]
    pub(super) fn compute_timex_metrics(t: &libc::timex) -> Vec<Metric> {
        // c_long / c_int → f64. Field magnitudes are bounded by the
        // kernel's PLL design (offsets are < ±0.5s when nanoseconds, and
        // counters are reset on calibration intervals); the precision
        // loss above 2^53 is not reachable in practice.
        #[allow(clippy::cast_precision_loss)]
        fn f(x: impl Into<i64>) -> f64 {
            let i: i64 = x.into();
            i as f64
        }

        let divisor = if (t.status & libc::STA_NANO) != 0 {
            NANO
        } else {
            MICRO
        };
        let sync_status = if (t.status & libc::STA_UNSYNC) == 0 {
            1.0
        } else {
            0.0
        };

        vec![
            gauge(
                "node_timex_sync_status",
                "Is clock synchronized to a reliable server (1 = yes, 0 = no).",
                sync_status,
            ),
            gauge(
                "node_timex_offset_seconds",
                "Time offset in between local system and reference clock.",
                f(t.offset) / divisor,
            ),
            gauge(
                "node_timex_frequency_adjustment_ratio",
                "Local clock frequency adjustment.",
                1.0 + f(t.freq) / PPM16_FRAC,
            ),
            gauge(
                "node_timex_maxerror_seconds",
                "Maximum error in seconds.",
                f(t.maxerror) / MICRO,
            ),
            gauge(
                "node_timex_estimated_error_seconds",
                "Estimated error in seconds.",
                f(t.esterror) / MICRO,
            ),
            gauge(
                "node_timex_status",
                "Value of the status array bits.",
                f(t.status),
            ),
            gauge(
                "node_timex_loop_time_constant",
                "Phase-locked loop time constant.",
                f(t.constant),
            ),
            gauge(
                "node_timex_tick_seconds",
                "Seconds between clock ticks.",
                f(t.tick) / MICRO,
            ),
            gauge(
                "node_timex_pps_frequency_hertz",
                "Pulse per second frequency.",
                f(t.ppsfreq) / PPM16_FRAC,
            ),
            gauge(
                "node_timex_pps_jitter_seconds",
                "Pulse per second jitter.",
                f(t.jitter) / divisor,
            ),
            gauge(
                "node_timex_pps_shift_seconds",
                "Pulse per second interval duration.",
                f(t.shift),
            ),
            gauge(
                "node_timex_pps_stability_hertz",
                "Pulse per second stability, average of recent frequency changes.",
                f(t.stabil) / PPM16_FRAC,
            ),
            counter(
                "node_timex_pps_jitter_total",
                "Pulse per second count of jitter limit exceeded events.",
                f(t.jitcnt),
            ),
            counter(
                "node_timex_pps_calibration_total",
                "Pulse per second count of calibration intervals.",
                f(t.calcnt),
            ),
            counter(
                "node_timex_pps_error_total",
                "Pulse per second count of calibration errors.",
                f(t.errcnt),
            ),
            counter(
                "node_timex_pps_stability_exceeded_total",
                "Pulse per second count of stability limit exceeded events.",
                f(t.stbcnt),
            ),
            gauge(
                "node_timex_tai_offset_seconds",
                "International Atomic Time (TAI) offset.",
                f(t.tai),
            ),
        ]
    }

    fn gauge(name: &str, help: &str, value: f64) -> Metric {
        Metric::new(name, help, MetricType::Gauge).with_sample(Sample::new(value))
    }

    fn counter(name: &str, help: &str, value: f64) -> Metric {
        Metric::new(name, help, MetricType::Counter).with_sample(Sample::new(value))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Hand-construct a `libc::timex` with known fields and verify
        /// every divisor + the `STA_NANO` branch.
        fn make_timex(status: libc::c_int) -> libc::timex {
            // SAFETY: `libc::timex` is plain-old-data; an all-zero
            // representation is a valid value of the type. We then
            // overwrite the fields we care about.
            let mut t: libc::timex = unsafe { std::mem::zeroed() };
            t.offset = 500_000; // microseconds → 0.5s OR nanoseconds → 0.0005s
            t.freq = 65_536_000_000; // ratio = 1 + 1.0 = 2.0
            t.maxerror = 250_000; // 0.25s
            t.esterror = 125_000; // 0.125s
            t.status = status;
            t.constant = 7;
            t.tick = 10_000; // 0.01s
            t.ppsfreq = 65_536_000_000; // 1_000_000.0
            t.jitter = 1_000;
            t.shift = 3;
            t.stabil = 65_536_000; // 1_000.0
            t.jitcnt = 1;
            t.calcnt = 2;
            t.errcnt = 3;
            t.stbcnt = 4;
            t.tai = 37;
            t
        }

        fn value(metrics: &[Metric], name: &str) -> f64 {
            metrics
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("metric {name} not found"))
                .samples[0]
                .value
        }

        #[test]
        fn microsecond_units_when_sta_nano_clear() {
            let t = make_timex(0); // STA_UNSYNC clear, STA_NANO clear
            let metrics = compute_timex_metrics(&t);
            assert_eq!(metrics.len(), 17);

            // offset = 500_000 / 1e6 = 0.5
            assert!((value(&metrics, "node_timex_offset_seconds") - 0.5).abs() < 1e-9);
            // jitter = 1_000 / 1e6 = 0.001
            assert!((value(&metrics, "node_timex_pps_jitter_seconds") - 0.001).abs() < 1e-9);
            // sync_status = 1 (STA_UNSYNC not set)
            assert!((value(&metrics, "node_timex_sync_status") - 1.0).abs() < f64::EPSILON);
            // freq ratio
            assert!((value(&metrics, "node_timex_frequency_adjustment_ratio") - 2.0).abs() < 1e-9);
            // tick = 10_000 / 1e6 = 0.01
            assert!((value(&metrics, "node_timex_tick_seconds") - 0.01).abs() < 1e-9);
        }

        #[test]
        fn nanosecond_units_when_sta_nano_set() {
            let t = make_timex(libc::STA_NANO);
            let metrics = compute_timex_metrics(&t);
            // offset = 500_000 / 1e9 = 5e-4
            assert!(
                (value(&metrics, "node_timex_offset_seconds") - 5e-4).abs() < 1e-12,
                "offset divisor should be ns when STA_NANO is set"
            );
            assert!(
                (value(&metrics, "node_timex_pps_jitter_seconds") - 1e-6).abs() < 1e-12,
                "jitter divisor should be ns when STA_NANO is set"
            );
        }

        #[test]
        fn sync_status_zero_when_sta_unsync_set() {
            let t = make_timex(libc::STA_UNSYNC);
            let metrics = compute_timex_metrics(&t);
            assert!((value(&metrics, "node_timex_sync_status") - 0.0).abs() < f64::EPSILON);
        }

        #[test]
        fn counter_typed_pps_event_metrics() {
            let t = make_timex(0);
            let metrics = compute_timex_metrics(&t);
            for name in [
                "node_timex_pps_jitter_total",
                "node_timex_pps_calibration_total",
                "node_timex_pps_error_total",
                "node_timex_pps_stability_exceeded_total",
            ] {
                let m = metrics.iter().find(|m| m.name == name).unwrap();
                assert_eq!(m.mtype, MetricType::Counter, "{name} should be Counter");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_emits_node_time_seconds() {
        let cfg = Config::default();
        let metrics = TimeCollector.collect(&cfg).unwrap();
        let now = metrics.iter().find(|m| m.name == "node_time_seconds");
        let now = now.expect("node_time_seconds must be emitted on every platform");
        assert_eq!(now.mtype, MetricType::Gauge);
        assert_eq!(now.samples.len(), 1);
        // Plausibility: 2020-01-01 < now < 2100-01-01.
        let v = now.samples[0].value;
        assert!(v > 1_577_836_800.0, "node_time_seconds = {v}, too small");
        assert!(v < 4_102_444_800.0, "node_time_seconds = {v}, too large");
    }

    #[test]
    fn collect_emits_zone_offset() {
        let cfg = Config::default();
        let metrics = TimeCollector.collect(&cfg).unwrap();
        // Every supported Unix exposes tm_gmtoff. Guarantee the metric.
        let zone = metrics
            .iter()
            .find(|m| m.name == "node_time_zone_offset_seconds")
            .expect("zone offset metric");
        assert_eq!(zone.samples.len(), 1);
        // Offsets range −12h … +14h.
        let off = zone.samples[0].value;
        assert!((-12.0 * 3600.0..=14.0 * 3600.0).contains(&off));
        // The label must be present and non-empty.
        let label = &zone.samples[0].labels[0];
        assert_eq!(label.0, "time_zone");
        assert!(!label.1.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn collect_emits_timex_family_on_linux() {
        let cfg = Config::default();
        let metrics = TimeCollector.collect(&cfg).unwrap();
        // Either adjtimex succeeded (17 timex metrics) or it failed
        // (0 timex metrics — we only log, don't error). Both are valid.
        let timex_count = metrics
            .iter()
            .filter(|m| m.name.starts_with("node_timex_"))
            .count();
        assert!(timex_count == 0 || timex_count == 17, "got {timex_count}");
    }
}
