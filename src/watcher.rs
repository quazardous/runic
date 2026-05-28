use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, watch};
use tokio::time::{timeout, Instant};
use tracing::{debug, warn};

use crate::config::Config;

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(100);

pub fn spawn(path: PathBuf, initial: Arc<Config>) -> Result<watch::Receiver<Arc<Config>>> {
    let (cfg_tx, cfg_rx) = watch::channel(initial);
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<()>();

    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = raw_tx.send(());
            }
        },
        notify::Config::default(),
    )
    .context("create notify watcher")?;

    watcher
        .watch(&path, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch path {}", path.display()))?;

    tokio::spawn(reload_loop(watcher, raw_rx, path, cfg_tx));
    Ok(cfg_rx)
}

async fn reload_loop(
    _watcher: RecommendedWatcher,
    mut raw_rx: mpsc::UnboundedReceiver<()>,
    path: PathBuf,
    cfg_tx: watch::Sender<Arc<Config>>,
) {
    while raw_rx.recv().await.is_some() {
        let deadline = Instant::now() + DEBOUNCE_WINDOW;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match timeout(remaining, raw_rx.recv()).await {
                Ok(Some(_)) => continue,
                _ => break,
            }
        }

        match Config::load(&path) {
            Ok(new_cfg) => {
                debug!(path = %path.display(), "config reloaded");
                if cfg_tx.send(Arc::new(new_cfg)).is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "config reload failed; keeping previous");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_yaml(path: &std::path::Path, upstream_host: &str) -> std::io::Result<()> {
        let yaml = format!(
            r#"listen:
  addr: "127.0.0.1:0"
  auth: none
upstream:
  kind: http_connect
  host: {upstream_host}
  port: 823
  auth:
    username_env: RUNIC_TEST_USER
    password_env: RUNIC_TEST_PASS
"#
        );
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(path)?;
        f.write_all(yaml.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_on_file_change() {
        std::env::set_var("RUNIC_TEST_USER", "u1");
        std::env::set_var("RUNIC_TEST_PASS", "p1");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("runic.yaml");
        write_yaml(&path, "gw.first.example").expect("write initial");

        let initial = Arc::new(Config::load(&path).expect("load initial"));
        assert_eq!(initial.upstream.host, "gw.first.example");

        let mut rx = spawn(path.clone(), initial).expect("spawn watcher");

        // Give the watcher a beat to register the watch before we mutate.
        tokio::time::sleep(Duration::from_millis(50)).await;

        write_yaml(&path, "gw.second.example").expect("write updated");

        tokio::time::timeout(Duration::from_millis(1500), rx.changed())
            .await
            .expect("watcher didn't push within 1.5s")
            .expect("watch channel closed unexpectedly");

        let updated = rx.borrow();
        assert_eq!(updated.upstream.host, "gw.second.example");
    }
}
