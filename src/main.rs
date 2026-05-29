mod admin;
mod config;
mod routing;
mod server;
mod store;
mod upstream;
mod watcher;

#[cfg(test)]
mod test_helpers;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

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

    let (cfg, admin_cfg) = config::Config::load_with_admin(&cli.config)?;
    let snapshot_path = store::default_snapshot_path();
    let (config_store, cfg_rx) = store::ConfigStore::new(cfg, snapshot_path);
    let config_store = Arc::new(Mutex::new(config_store));

    watcher::spawn(cli.config.clone(), config_store.clone())?;
    admin::spawn(admin_cfg.addr, config_store.clone()).await?;

    server::run(cfg_rx).await
}
