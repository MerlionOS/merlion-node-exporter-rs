use std::collections::HashSet;

use anyhow::Context;
use clap::Parser;

use merlion_node_exporter_rs::cli::Cli;
use merlion_node_exporter_rs::collectors;
use merlion_node_exporter_rs::config::Config;
use merlion_node_exporter_rs::registry::Registry;
use merlion_node_exporter_rs::server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let config = Config::new(cli.procfs.clone(), cli.sysfs.clone(), cli.rootfs.clone())
        .with_textfile_directory(cli.textfile_directory.clone());

    let registry = build_registry(&cli.only_collectors, &cli.no_collector);
    tracing::info!(
        collectors = ?registry.enabled_names(),
        "enabled collectors",
    );

    let address = cli.resolved_listen_address();
    server::serve(&address, &cli.telemetry_path, registry, config)
        .await
        .context("HTTP server exited with error")?;
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn build_registry(only: &[String], disabled: &[String]) -> Registry {
    let only: HashSet<&str> = only.iter().map(String::as_str).collect();
    let disabled: HashSet<&str> = disabled.iter().map(String::as_str).collect();

    let mut registry = Registry::new();
    for c in collectors::all() {
        let name = c.name();
        if !only.is_empty() && !only.contains(name) {
            continue;
        }
        if disabled.contains(name) {
            continue;
        }
        registry.register(c);
    }
    registry
}
