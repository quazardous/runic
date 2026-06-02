//! Thin CLI shell over the `runic` core library. The planned Windows tray app
//! (`runic-tray`) is a second shell over the same library.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use runic::{admin, config, server, silo, store, watcher};

/// How long a decrypted variation stays warm in RAM after its last use before the
/// keep-alive cache evicts it (and drops its plaintext config from memory).
const SILO_IDLE_TTL_SECS: u64 = 300;

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
    let snapshot_path = store::default_snapshot_path();

    // Silo mode (opt-in): an encrypted per-variation config store, opened next to
    // the snapshot. Shared with the admin API (and, in a later slice, the data
    // plane). Absent ⇒ plain mode, unchanged.
    let silo = match silo_cfg.as_ref().filter(|s| s.enabled) {
        Some(s) => {
            let dir = snapshot_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("runic.silo");
            let cache = silo::VariationCache::new(
                silo::SiloStore::open(dir, s.ttl_days * 86_400)?,
                SILO_IDLE_TTL_SECS,
            );
            tracing::info!(auth = ?s.auth, ttl_days = s.ttl_days, "config silo mode enabled");
            Some(Arc::new(Mutex::new(cache)))
        }
        None => None,
    };

    let (config_store, cfg_rx) = store::ConfigStore::new(cfg, snapshot_path);
    let config_store = Arc::new(Mutex::new(config_store));

    watcher::spawn(cli.config.clone(), config_store.clone())?;
    admin::spawn(admin_cfg.addr, config_store.clone(), silo.clone()).await?;

    server::run(cfg_rx, silo).await
}
