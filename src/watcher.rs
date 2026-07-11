use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{timeout, Instant};
use tracing::{debug, warn};

use crate::config::Config;
use crate::store::ConfigStore;

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(100);

/// Watch the cold YAML and feed reloads into the shared [`ConfigStore`]. The
/// store owns the broadcast channel, so a cold reload and an admin mutation
/// converge on the same apply path ([`ConfigStore::publish`]).
///
/// A file that doesn't exist yet (tolerated-missing default config) is watched
/// through its **parent directory** — notify can't watch a non-existent path —
/// filtered to events touching our file name: create the config after boot and
/// it hot-loads like any edit, no restart. If the parent doesn't exist either,
/// hot reload is disabled with a warning rather than failing the boot (the
/// admin API still drives the instance).
pub fn spawn(path: PathBuf, store: Arc<Mutex<ConfigStore>>) -> Result<()> {
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<()>();

    // The filter only applies in parent-watch mode: directory events carry the
    // paths of the entries that changed, and neighbours are none of our business.
    let file_name = path.file_name().map(|n| n.to_os_string());
    let watch_parent = !path.exists();
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let ours = !watch_parent
                    || ev
                        .paths
                        .iter()
                        .any(|p| p.file_name() == file_name.as_deref());
                if ours {
                    let _ = raw_tx.send(());
                }
            }
        },
        notify::Config::default(),
    )
    .context("create notify watcher")?;

    let target = if watch_parent {
        match path.parent().filter(|d| d.exists()) {
            Some(dir) => dir.to_path_buf(),
            None => {
                warn!(
                    path = %path.display(),
                    "config parent directory does not exist; hot reload disabled \
                     (restart after creating the config, or drive via the admin API)"
                );
                return Ok(());
            }
        }
    } else {
        path.clone()
    };
    watcher
        .watch(&target, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch path {}", target.display()))?;

    tokio::spawn(reload_loop(watcher, raw_rx, path, store));
    Ok(())
}

async fn reload_loop(
    _watcher: RecommendedWatcher,
    mut raw_rx: mpsc::UnboundedReceiver<()>,
    path: PathBuf,
    store: Arc<Mutex<ConfigStore>>,
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
                debug!(path = %path.display(), "cold config reloaded");
                store.lock().await.set_cold(new_cfg);
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "cold config reload failed; keeping previous");
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
upstreams:
  default:
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
    async fn reload_on_file_change_updates_store() {
        std::env::set_var("RUNIC_TEST_USER", "u1");
        std::env::set_var("RUNIC_TEST_PASS", "p1");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("runic.yaml");
        write_yaml(&path, "gw.first.example").expect("write initial");

        let initial = Config::load(&path).expect("load initial");
        assert_eq!(initial.default_upstream().unwrap().host, "gw.first.example");

        let (store, mut rx) = ConfigStore::new(initial, dir.path().join("runic.snapshot.json"));
        let store = Arc::new(Mutex::new(store));

        spawn(path.clone(), store.clone()).expect("spawn watcher");

        // Give the watcher a beat to register the watch before we mutate.
        tokio::time::sleep(Duration::from_millis(50)).await;

        write_yaml(&path, "gw.second.example").expect("write updated");

        tokio::time::timeout(Duration::from_millis(1500), rx.changed())
            .await
            .expect("watcher didn't push within 1.5s")
            .expect("watch channel closed unexpectedly");

        let updated = rx.borrow();
        assert_eq!(
            updated.default_upstream().unwrap().host,
            "gw.second.example"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn config_created_after_boot_hot_loads() {
        std::env::set_var("RUNIC_TEST_USER", "u1");
        std::env::set_var("RUNIC_TEST_PASS", "p1");

        // Tolerated-missing scenario: boot on defaults with NO config file,
        // then create it — the parent-directory watch must pick it up.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("runic.yaml");
        assert!(!path.exists());

        let initial = Config::load_with_admin_opts(&path, true)
            .expect("missing file tolerated")
            .0;
        assert!(initial.upstreams.is_empty(), "boots on defaults");

        let (store, mut rx) = ConfigStore::new(initial, dir.path().join("runic.snapshot.json"));
        let store = Arc::new(Mutex::new(store));

        spawn(path.clone(), store.clone()).expect("spawn watcher on missing file");
        tokio::time::sleep(Duration::from_millis(50)).await;

        write_yaml(&path, "gw.created.example").expect("create config after boot");

        tokio::time::timeout(Duration::from_millis(1500), rx.changed())
            .await
            .expect("watcher didn't pick up the created file within 1.5s")
            .expect("watch channel closed unexpectedly");

        let updated = rx.borrow();
        assert_eq!(
            updated.default_upstream().unwrap().host,
            "gw.created.example"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_parent_dir_disables_watch_without_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("no-such-subdir").join("runic.yaml");
        let cfg = Config::load_with_admin_opts(&path, true)
            .expect("tolerated")
            .0;
        let (store, _rx) = ConfigStore::new(cfg, dir.path().join("runic.snapshot.json"));
        // Boot must survive: no watch target, but no error either.
        spawn(path, Arc::new(Mutex::new(store))).expect("spawn degrades gracefully");
    }
}
