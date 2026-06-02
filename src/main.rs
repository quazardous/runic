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

/// How often the background sweeper evicts idle warm variations and runs the
/// disk decay GC.
const SILO_SWEEP_INTERVAL_SECS: u64 = 60;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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

    // Silo mode (opt-in): the encrypted per-variation config store + its default
    // binding mode. Absent ⇒ plain mode, unchanged.
    let silo = match silo_cfg.as_ref().filter(|s| s.enabled) {
        Some(s) => {
            let dir = snapshot_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("runic.silo");
            let cache = Arc::new(Mutex::new(silo::VariationCache::new(
                silo::SiloStore::open(dir, s.ttl_days * 86_400)?,
                SILO_IDLE_TTL_SECS,
            )));
            tracing::info!(auth = ?s.auth, ttl_days = s.ttl_days, "config silo mode enabled");
            Some((cache, s.auth))
        }
        None => None,
    };

    let (config_store, cfg_rx) = store::ConfigStore::new(cfg, snapshot_path);
    let config_store = Arc::new(Mutex::new(config_store));

    // Silo runtime: the `none`-mode port registry + the admin handle, plus a
    // background sweeper that evicts idle variations and tears down their ports.
    let silo_admin = silo.as_ref().map(|(cache, mode)| {
        let ports = server::SiloPorts::new(cfg_rx.clone(), cache.clone());
        admin::SiloAdmin {
            cache: cache.clone(),
            ports,
            default_mode: *mode,
        }
    });
    if let Some(sa) = silo_admin.as_ref() {
        let cache = sa.cache.clone();
        let ports = sa.ports.clone();
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(SILO_SWEEP_INTERVAL_SECS));
            loop {
                tick.tick().await;
                let swept = { cache.lock().await.sweep(unix_now()) };
                match swept {
                    Ok((evicted, purged)) if !evicted.is_empty() || purged > 0 => {
                        ports.close(&evicted).await;
                        tracing::debug!(evicted = evicted.len(), purged, "silo sweep");
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!(error = %err, "silo sweep failed"),
                }
            }
        });
    }

    watcher::spawn(cli.config.clone(), config_store.clone())?;
    admin::spawn(admin_cfg.addr, config_store.clone(), silo_admin).await?;

    let server_silo = silo.map(|(cache, _)| cache);
    server::run(cfg_rx, server_silo).await
}
