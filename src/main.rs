//! Thin CLI shell over the `runic` core library. The planned Windows tray app
//! (`runic-tray`) is a second shell over the same library.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use runic::{admin, config, server, store, watcher};

#[derive(Parser, Debug)]
#[command(
    name = "runic",
    version,
    about = "Local SOCKS5 proxy relaying via HTTP CONNECT upstream"
)]
struct Cli {
    #[arg(short, long, default_value = "/etc/runic/runic.yaml")]
    config: PathBuf,

    #[arg(long, env = "RUNIC_LOG", default_value = "runic=info")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log)?)
        .with_target(true)
        .init();

    let (cfg, admin_cfg, silo_cfg) = config::Config::load_with_admin(&cli.config)?;
    if let Some(silo) = silo_cfg.as_ref().filter(|s| s.enabled) {
        // Binding layer (variation serving) is wired in a following step; for now
        // surface that silo mode is selected and which client-binding it expects.
        tracing::info!(
            auth = ?silo.auth,
            ttl_days = silo.ttl_days,
            "config silo mode enabled"
        );
    }
    let snapshot_path = store::default_snapshot_path();
    let (config_store, cfg_rx) = store::ConfigStore::new(cfg, snapshot_path);
    let config_store = Arc::new(Mutex::new(config_store));

    watcher::spawn(cli.config.clone(), config_store.clone())?;
    admin::spawn(admin_cfg.addr, config_store.clone()).await?;

    server::run(cfg_rx).await
}
