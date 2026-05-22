//! Built-in metric collectors.
//!
//! Each submodule implements [`Collector`] for one logical metric source
//! (`/proc/loadavg`, `/proc/meminfo`, `uname` syscall, …). Collectors are
//! cheap to construct and stateless; the same instance handles every scrape.

use crate::registry::Collector;

pub mod cpu;
pub mod filesystem;
pub mod loadavg;
pub mod meminfo;
pub mod stat;
pub mod netstat;
pub mod pressure;
pub mod sockstat;
pub mod uname;
pub mod vmstat;

pub type BoxedCollector = Box<dyn Collector>;

/// Returns every collector registered by default at this point in the
/// project. Order is significant only for the output ordering of metric
/// families — not for correctness.
pub fn all() -> Vec<BoxedCollector> {
    vec![
        Box::new(cpu::CpuCollector),
        Box::new(filesystem::FilesystemCollector),
        Box::new(loadavg::LoadavgCollector),
        Box::new(meminfo::MeminfoCollector),
        Box::new(stat::StatCollector),
        Box::new(netstat::NetstatCollector),
        Box::new(pressure::PressureCollector),
        Box::new(sockstat::SockstatCollector),
        Box::new(uname::UnameCollector),
        Box::new(vmstat::VmstatCollector),
    ]
}
