//! Command-line interface.
//!
//! Flag names mirror upstream `node_exporter` where practical so existing
//! deployment tooling (Ansible roles, systemd units) can be reused without
//! translation.

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "merlion-node-exporter",
    version,
    about = "Prometheus exporter for hardware and OS metrics — Rust reimplementation of node_exporter."
)]
pub struct Cli {
    /// Address on which to expose metrics and the web interface.
    #[arg(
        long = "web.listen-address",
        default_value = ":9100",
        env = "MNE_LISTEN_ADDRESS"
    )]
    pub listen_address: String,

    /// Path under which to expose metrics.
    #[arg(
        long = "web.telemetry-path",
        default_value = "/metrics",
        env = "MNE_TELEMETRY_PATH"
    )]
    pub telemetry_path: String,

    /// procfs mountpoint.
    #[arg(long = "path.procfs", default_value = "/proc", env = "MNE_PROCFS")]
    pub procfs: PathBuf,

    /// sysfs mountpoint.
    #[arg(long = "path.sysfs", default_value = "/sys", env = "MNE_SYSFS")]
    pub sysfs: PathBuf,

    /// Rootfs path (prefix applied to absolute mountpoints, e.g. filesystem collector).
    #[arg(long = "path.rootfs", default_value = "/", env = "MNE_ROOTFS")]
    pub rootfs: PathBuf,

    /// Directory of `*.prom` files served by the textfile collector.
    /// When unset the textfile collector emits nothing — matching upstream.
    #[arg(
        long = "collector.textfile.directory",
        value_name = "DIR",
        env = "MNE_TEXTFILE_DIRECTORY"
    )]
    pub textfile_directory: Option<PathBuf>,

    /// Disable a collector by name. May be passed multiple times.
    #[arg(long = "no-collector", value_name = "NAME")]
    pub no_collector: Vec<String>,

    /// Enable only the named collectors (overrides defaults). May be passed multiple times.
    #[arg(long = "collector.only", value_name = "NAME")]
    pub only_collectors: Vec<String>,
}

impl Cli {
    /// Resolve `:9100` -> `0.0.0.0:9100`, leave host-qualified strings intact.
    pub fn resolved_listen_address(&self) -> String {
        if let Some(stripped) = self.listen_address.strip_prefix(':') {
            format!("0.0.0.0:{stripped}")
        } else {
            self.listen_address.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_colon_prefixed_address() {
        let cli = Cli::parse_from(["bin", "--web.listen-address", ":9100"]);
        assert_eq!(cli.resolved_listen_address(), "0.0.0.0:9100");
    }

    #[test]
    fn leaves_qualified_address_intact() {
        let cli = Cli::parse_from(["bin", "--web.listen-address", "127.0.0.1:9100"]);
        assert_eq!(cli.resolved_listen_address(), "127.0.0.1:9100");
    }
}
