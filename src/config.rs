//! Runtime configuration shared with every collector.
//!
//! Mirrors `node_exporter`'s `--path.procfs`, `--path.sysfs`, `--path.rootfs`
//! flags so collectors can be exercised against fixture trees in tests and
//! against bind-mounted host filesystems inside containers.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub procfs: PathBuf,
    pub sysfs: PathBuf,
    pub rootfs: PathBuf,
    /// Directory scanned by the `textfile` collector for `*.prom` files.
    /// `None` disables the collector — matching upstream's behaviour when
    /// `--collector.textfile.directory` is unset.
    pub textfile_directory: Option<PathBuf>,
}

impl Config {
    pub fn new(procfs: PathBuf, sysfs: PathBuf, rootfs: PathBuf) -> Self {
        Self {
            procfs,
            sysfs,
            rootfs,
            textfile_directory: None,
        }
    }

    #[must_use]
    pub fn with_textfile_directory(mut self, dir: Option<PathBuf>) -> Self {
        self.textfile_directory = dir;
        self
    }

    /// Resolve a path beneath `procfs`. Accepts both `meminfo` and `/proc/meminfo`
    /// — leading `/proc` is stripped so callers can use the upstream-style path.
    pub fn proc_path(&self, rel: &str) -> PathBuf {
        join_root(
            &self.procfs,
            rel.trim_start_matches("/proc").trim_start_matches('/'),
        )
    }

    pub fn sys_path(&self, rel: &str) -> PathBuf {
        join_root(
            &self.sysfs,
            rel.trim_start_matches("/sys").trim_start_matches('/'),
        )
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new(
            PathBuf::from("/proc"),
            PathBuf::from("/sys"),
            PathBuf::from("/"),
        )
    }
}

fn join_root(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_path_strips_proc_prefix() {
        let c = Config::new("/tmp/procfs".into(), "/sys".into(), "/".into());
        assert_eq!(
            c.proc_path("/proc/meminfo"),
            PathBuf::from("/tmp/procfs/meminfo")
        );
        assert_eq!(c.proc_path("meminfo"), PathBuf::from("/tmp/procfs/meminfo"));
    }

    #[test]
    fn sys_path_strips_sys_prefix() {
        let c = Config::new("/proc".into(), "/tmp/sysfs".into(), "/".into());
        assert_eq!(
            c.sys_path("/sys/class/thermal"),
            PathBuf::from("/tmp/sysfs/class/thermal"),
        );
    }
}
