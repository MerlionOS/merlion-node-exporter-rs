//! `uname(2)` — kernel / hostname / architecture identifier metric.
//!
//! Emits a single `node_uname_info{...} 1` info-style metric with one label
//! per `utsname` field, matching the convention used by upstream
//! `node_exporter` and by `kube-state-metrics`.

use std::ffi::CStr;
use std::mem::MaybeUninit;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct UnameCollector;

impl Collector for UnameCollector {
    fn name(&self) -> &'static str {
        "uname"
    }

    fn collect(&self, _cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let info = read_uname().context("uname syscall failed")?;
        let sample = Sample::new(1.0)
            .with_label("sysname", info.sysname)
            .with_label("release", info.release)
            .with_label("version", info.version)
            .with_label("machine", info.machine)
            .with_label("nodename", info.nodename)
            .with_label("domainname", info.domainname);
        Ok(vec![
            Metric::new(
                "node_uname_info",
                "Labeled system information as provided by the uname system call.",
                MetricType::Gauge,
            )
            .with_sample(sample),
        ])
    }
}

#[derive(Debug, Clone)]
struct UnameInfo {
    sysname: String,
    nodename: String,
    release: String,
    version: String,
    machine: String,
    domainname: String,
}

fn read_uname() -> anyhow::Result<UnameInfo> {
    // SAFETY: `libc::utsname` is plain-old-data; `libc::uname` writes into
    // the buffer and returns 0 on success. We treat any non-zero return as
    // failure and never read uninitialised memory.
    let mut buf = MaybeUninit::<libc::utsname>::uninit();
    let rc = unsafe { libc::uname(buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(anyhow!(std::io::Error::last_os_error()));
    }
    let buf = unsafe { buf.assume_init() };

    Ok(UnameInfo {
        sysname: c_to_string(&buf.sysname),
        nodename: c_to_string(&buf.nodename),
        release: c_to_string(&buf.release),
        version: c_to_string(&buf.version),
        machine: c_to_string(&buf.machine),
        domainname: domainname(&buf),
    })
}

#[cfg(target_os = "linux")]
fn domainname(buf: &libc::utsname) -> String {
    c_to_string(&buf.domainname)
}

#[cfg(not(target_os = "linux"))]
fn domainname(_buf: &libc::utsname) -> String {
    // Non-Linux utsname has no domainname field; report "(none)" to match
    // upstream node_exporter behaviour on those platforms.
    String::from("(none)")
}

fn c_to_string(field: &[libc::c_char]) -> String {
    // The kernel writes a NUL-terminated string into each fixed-size array.
    let ptr = field.as_ptr();
    // SAFETY: `field` is a fixed-size array whose contents the kernel filled
    // with a NUL-terminated C string; `CStr::from_ptr` walks until it finds
    // that terminator.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_returns_one_info_metric() {
        let cfg = Config::default();
        let metrics = UnameCollector.collect(&cfg).unwrap();
        assert_eq!(metrics.len(), 1);
        let m = &metrics[0];
        assert_eq!(m.name, "node_uname_info");
        assert_eq!(m.samples.len(), 1);
        assert!((m.samples[0].value - 1.0).abs() < f64::EPSILON);
        let label_names: Vec<&str> = m.samples[0]
            .labels
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert!(label_names.contains(&"sysname"));
        assert!(label_names.contains(&"machine"));
    }
}
