//! Three-layer config store with a firewalld-style runtime/permanent split.
//!
//! Layering — runtime/permanent split modelled on firewalld and HAProxy's
//! server state-file (`show servers state` ↔ `load-server-state-from-file`):
//!
//! | Layer        | Owner            | Lifetime                          |
//! |--------------|------------------|-----------------------------------|
//! | `cold`       | sysadmin (YAML)  | reloaded from disk on file change |
//! | `snapshot`   | operator/agent   | persisted JSON cache, survives restart |
//! | `hot`        | operator/agent   | RAM only, lost on restart         |
//!
//! The snapshot is a "dumb persistent cache" (not a source of truth): a
//! versioned, round-trippable dump, last-write-wins, no history/rollback.
//!
//! Single apply path: every mutation — whether from the file watcher (cold) or
//! the admin API (hot/permanent) — ends in [`ConfigStore::publish`], which
//! rebuilds the merged [`Config`] and broadcasts it over one `watch` channel
//! (pattern borrowed from gost's `config.Set` + reconcile).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::config::{Config, Upstream};

/// On-disk snapshot schema version. Bump on a breaking format change; older
/// files are tolerated (logged) so a downgrade never hard-fails the boot.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Which layer a merged upstream effectively comes from (for `/v1/diagnose`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Cold,
    Snapshot,
    Hot,
}

/// Versioned snapshot file — the format written is exactly the format read and
/// the one `/v1/config` exposes (HAProxy state-file round-trip property).
#[derive(Debug, Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    upstreams: BTreeMap<String, Upstream>,
}

/// One row of the layer diff (`/v1/diff`): a name defined in more than one
/// layer, so the operator can see what shadows what.
#[derive(Debug, Clone, Serialize)]
pub struct DiffEntry {
    pub name: String,
    pub cold: Option<Upstream>,
    pub snapshot: Option<Upstream>,
    pub hot: Option<Upstream>,
    /// The layer that wins under the current precedence.
    pub effective: Source,
    /// True if the defining layers don't all agree on the value.
    pub diverges: bool,
}

pub struct ConfigStore {
    cold: Arc<Config>,
    snapshot: BTreeMap<String, Upstream>,
    hot: BTreeMap<String, Upstream>,
    /// Active-route pointer (admin `PUT /v1/route/default`). Runtime only — not
    /// persisted to the snapshot. `None` falls back to the `default` entry.
    active_route: Option<String>,
    snapshot_path: PathBuf,
    cfg_tx: watch::Sender<Arc<Config>>,
}

impl ConfigStore {
    /// Build a store from the cold config, loading the snapshot file if present,
    /// and return the receiver the data plane reads from.
    pub fn new(cold: Config, snapshot_path: PathBuf) -> (Self, watch::Receiver<Arc<Config>>) {
        let snapshot = match load_snapshot(&snapshot_path) {
            Ok(Some(map)) => {
                info!(path = %snapshot_path.display(), entries = map.len(), "snapshot loaded");
                map
            }
            Ok(None) => BTreeMap::new(),
            Err(e) => {
                warn!(path = %snapshot_path.display(), error = %e, "snapshot load failed; starting from cold only");
                BTreeMap::new()
            }
        };
        let hot = BTreeMap::new();
        let active_route: Option<String> = None;
        let merged = merge(&cold, &snapshot, &hot, &active_route);
        let (cfg_tx, cfg_rx) = watch::channel(Arc::new(merged));
        let store = Self {
            cold: Arc::new(cold),
            snapshot,
            hot,
            active_route,
            snapshot_path,
            cfg_tx,
        };
        (store, cfg_rx)
    }

    /// Rebuild the merged config and broadcast it. The one apply path.
    fn publish(&self) {
        let merged = merge(&self.cold, &self.snapshot, &self.hot, &self.active_route);
        // send_replace ignores the "no receivers" case — the data plane may not
        // be up yet during boot, and that's fine.
        let _ = self.cfg_tx.send(Arc::new(merged));
    }

    /// Replace the cold layer (file watcher reload) and republish.
    pub fn set_cold(&mut self, cold: Config) {
        self.cold = Arc::new(cold);
        self.publish();
    }

    /// Runtime mutation: hot layer only, not persisted.
    pub fn apply_runtime(&mut self, name: String, up: Upstream) {
        self.hot.insert(name, up);
        self.publish();
    }

    /// Permanent mutation: write to the snapshot (persisted) and clear any
    /// shadowing hot entry so the permanent value is the effective one. Active
    /// immediately via the merge.
    pub fn apply_permanent(&mut self, name: String, up: Upstream) -> Result<()> {
        self.hot.remove(&name);
        self.snapshot.insert(name, up);
        self.persist_snapshot()?;
        self.publish();
        Ok(())
    }

    /// Remove from the hot layer; the name falls back to snapshot or cold.
    pub fn remove_runtime(&mut self, name: &str) -> bool {
        let existed = self.hot.remove(name).is_some();
        if existed {
            self.publish();
        }
        existed
    }

    /// Remove from both hot and snapshot; the name falls back to cold.
    pub fn remove_permanent(&mut self, name: &str) -> Result<bool> {
        let in_hot = self.hot.remove(name).is_some();
        let in_snap = self.snapshot.remove(name).is_some();
        if in_snap {
            self.persist_snapshot()?;
        }
        if in_hot || in_snap {
            self.publish();
        }
        Ok(in_hot || in_snap)
    }

    /// firewalld `--runtime-to-permanent`: fold the whole hot layer into the
    /// snapshot, persist, and clear hot.
    pub fn promote_runtime_to_permanent(&mut self) -> Result<usize> {
        let n = self.hot.len();
        for (name, up) in std::mem::take(&mut self.hot) {
            self.snapshot.insert(name, up);
        }
        self.persist_snapshot()?;
        self.publish();
        Ok(n)
    }

    /// Drop the snapshot entirely (map + file). Next boot = cold YAML only.
    pub fn wipe_snapshot(&mut self) -> Result<()> {
        self.snapshot.clear();
        match std::fs::remove_file(&self.snapshot_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("remove snapshot {}", self.snapshot_path.display()));
            }
        }
        self.publish();
        Ok(())
    }

    /// Per-name effective source, for `/v1/diagnose`.
    pub fn diagnose(&self) -> BTreeMap<String, Source> {
        let mut out = BTreeMap::new();
        for name in self
            .cold
            .upstreams
            .keys()
            .chain(self.snapshot.keys())
            .chain(self.hot.keys())
        {
            if out.contains_key(name) {
                continue;
            }
            if let Some(src) = self.source_of(name) {
                out.insert(name.clone(), src);
            }
        }
        out
    }

    /// Layer diff for `/v1/diff`: every name present in more than one layer.
    pub fn diff(&self) -> Vec<DiffEntry> {
        let mut names: Vec<&String> = self
            .cold
            .upstreams
            .keys()
            .chain(self.snapshot.keys())
            .chain(self.hot.keys())
            .collect();
        names.sort();
        names.dedup();

        let mut out = Vec::new();
        for name in names {
            let cold = self.cold.upstreams.get(name);
            let snap = self.snapshot.get(name);
            let hot = self.hot.get(name);
            let defined = [cold.is_some(), snap.is_some(), hot.is_some()]
                .iter()
                .filter(|b| **b)
                .count();
            if defined < 2 {
                continue; // only surface multi-layer (shadowing) cases
            }
            let present: Vec<&Upstream> = [cold, snap, hot].into_iter().flatten().collect();
            let diverges = present.windows(2).any(|w| w[0] != w[1]);
            out.push(DiffEntry {
                name: name.clone(),
                cold: cold.cloned(),
                snapshot: snap.cloned(),
                hot: hot.cloned(),
                effective: self.source_of(name).expect("defined in some layer"),
                diverges,
            });
        }
        out
    }

    /// The merged effective config (`/v1/config`).
    pub fn merged(&self) -> Config {
        merge(&self.cold, &self.snapshot, &self.hot, &self.active_route)
    }

    /// The **hot** layer only (runtime-pushed, RAM-only upstreams). The status
    /// surface shows this — the live runtime view, not the cold YAML base.
    pub fn hot(&self) -> &BTreeMap<String, Upstream> {
        &self.hot
    }

    /// Set (or clear with `None`) the active-route pointer and republish.
    /// Runtime only — not persisted to the snapshot. `None` falls back to the
    /// entry named `default`.
    pub fn set_active_route(&mut self, route: Option<String>) {
        self.active_route = route;
        self.publish();
    }

    /// The current active-route pointer, if set.
    pub fn active_route(&self) -> Option<&str> {
        self.active_route.as_deref()
    }

    pub fn pool_size(&self) -> usize {
        self.merged().upstreams.len()
    }

    fn source_of(&self, name: &str) -> Option<Source> {
        // Mirrors the precedence in `merge`: the first layer (highest priority)
        // that defines the name is the effective source.
        if self.hot.contains_key(name) {
            Some(Source::Hot)
        } else if self.snapshot.contains_key(name) {
            Some(Source::Snapshot)
        } else if self.cold.upstreams.contains_key(name) {
            Some(Source::Cold)
        } else {
            None
        }
    }

    fn persist_snapshot(&self) -> Result<()> {
        write_snapshot(&self.snapshot_path, &self.snapshot)
    }
}

/// Merge the three layers into the effective pool.
///
/// PRECEDENCE = hot > snapshot > cold: snapshot wins over cold on a name
/// conflict, coherent with firewalld's `--permanent`.
///
/// NOTE — this precedence is a deliberate, isolated policy choice. The HAProxy
/// state-file model takes the opposite stance (the config file wins over the
/// saved state), which avoids a stale snapshot shadowing a later sysadmin YAML
/// fix. To switch to "snapshot only ADDs, cold authoritative on conflict",
/// this is the single site to change: skip the snapshot overlay for names
/// already present in cold.
fn merge(
    cold: &Config,
    snapshot: &BTreeMap<String, Upstream>,
    hot: &BTreeMap<String, Upstream>,
    active_route: &Option<String>,
) -> Config {
    let mut upstreams = cold.upstreams.clone();
    for (k, v) in snapshot {
        upstreams.insert(k.clone(), v.clone());
    }
    for (k, v) in hot {
        upstreams.insert(k.clone(), v.clone());
    }
    Config {
        listen: cold.listen.clone(),
        upstreams,
        active_route: active_route.clone(),
    }
}

/// Resolve the default snapshot path under the per-user config dir:
/// `%APPDATA%\runic\runic.snapshot.json` on Windows,
/// `$XDG_CONFIG_HOME/runic/runic.snapshot.json` (or `$HOME/.config/runic/...`,
/// or `./runic/...`) on Unix. See [`crate::paths::config_dir`].
pub fn default_snapshot_path() -> PathBuf {
    crate::paths::config_dir().join("runic.snapshot.json")
}

/// Load the snapshot if the file exists. `Ok(None)` = no file (fresh boot).
fn load_snapshot(path: &Path) -> Result<Option<BTreeMap<String, Upstream>>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read snapshot {}", path.display())),
    };
    let file: SnapshotFile =
        serde_json::from_str(&raw).with_context(|| format!("parse snapshot {}", path.display()))?;
    if file.version != SNAPSHOT_VERSION {
        warn!(
            found = file.version,
            expected = SNAPSHOT_VERSION,
            "snapshot version mismatch; loading best-effort"
        );
    }
    Ok(Some(file.upstreams))
}

/// Atomic-replace write: temp file in the same dir (mode 600), fsync, rename.
fn write_snapshot(path: &Path, upstreams: &BTreeMap<String, Upstream>) -> Result<()> {
    use std::io::Write;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create snapshot dir {}", dir.display()))?;
    }

    let file = SnapshotFile {
        version: SNAPSHOT_VERSION,
        upstreams: upstreams.clone(),
    };
    let body = serde_json::to_vec_pretty(&file).context("serialize snapshot")?;

    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("open temp snapshot {}", tmp.display()))?;
        f.write_all(&body)
            .with_context(|| format!("write temp snapshot {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync temp snapshot {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    debug!(path = %path.display(), entries = upstreams.len(), "snapshot persisted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Listen, ListenAuth, UpstreamCreds, UpstreamKind};
    use std::net::SocketAddr;

    fn up(host: &str) -> Upstream {
        Upstream {
            kind: UpstreamKind::HttpConnect,
            host: host.to_string(),
            port: 823,
            auth: UpstreamCreds {
                username: "u".into(),
                password: "p".into(),
            },
        }
    }

    fn cold_with(entries: &[(&str, &str)]) -> Config {
        let mut upstreams = BTreeMap::new();
        for (name, host) in entries {
            upstreams.insert((*name).to_string(), up(host));
        }
        Config {
            listen: Listen {
                addr: "127.0.0.1:7777".parse::<SocketAddr>().unwrap(),
                auth: ListenAuth::None,
            },
            upstreams,
            active_route: None,
        }
    }

    fn store_with(
        cold: Config,
        dir: &tempfile::TempDir,
    ) -> (ConfigStore, watch::Receiver<Arc<Config>>) {
        let path = dir.path().join("runic.snapshot.json");
        ConfigStore::new(cold, path)
    }

    #[test]
    fn merge_precedence_hot_over_snapshot_over_cold() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);

        // snapshot shadows cold
        store.snapshot.insert("default".into(), up("snap.example"));
        assert_eq!(store.merged().upstreams["default"].host, "snap.example");
        assert_eq!(store.diagnose()["default"], Source::Snapshot);

        // hot shadows snapshot + cold
        store.hot.insert("default".into(), up("hot.example"));
        assert_eq!(store.merged().upstreams["default"].host, "hot.example");
        assert_eq!(store.diagnose()["default"], Source::Hot);
    }

    #[test]
    fn apply_permanent_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);

        store
            .apply_permanent("us".into(), up("us.example"))
            .unwrap();

        // reload from disk in a fresh store
        let (reloaded, _rx2) = store_with(cold_with(&[("default", "cold.example")]), &dir);
        assert_eq!(reloaded.merged().upstreams["us"].host, "us.example");
        assert_eq!(reloaded.diagnose()["us"], Source::Snapshot);
    }

    #[test]
    fn apply_runtime_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);

        store.apply_runtime("us".into(), up("us.example"));
        assert_eq!(store.diagnose()["us"], Source::Hot);

        // fresh store from same dir: the runtime entry is gone
        let (reloaded, _rx2) = store_with(cold_with(&[("default", "cold.example")]), &dir);
        assert!(!reloaded.merged().upstreams.contains_key("us"));
    }

    #[test]
    fn wipe_snapshot_falls_back_to_cold() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);

        store
            .apply_permanent("default".into(), up("snap.example"))
            .unwrap();
        assert_eq!(store.merged().upstreams["default"].host, "snap.example");

        store.wipe_snapshot().unwrap();
        assert_eq!(store.merged().upstreams["default"].host, "cold.example");
        assert!(!store.snapshot_path.exists());
    }

    #[test]
    fn promote_dumps_hot_to_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);

        store.apply_runtime("us".into(), up("us.example"));
        let n = store.promote_runtime_to_permanent().unwrap();
        assert_eq!(n, 1);
        assert!(store.hot.is_empty());

        let (reloaded, _rx2) = store_with(cold_with(&[("default", "cold.example")]), &dir);
        assert_eq!(reloaded.diagnose()["us"], Source::Snapshot);
    }

    #[test]
    fn apply_permanent_is_atomic_replace_last_write_wins() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);

        store
            .apply_permanent("us".into(), up("first.example"))
            .unwrap();
        store
            .apply_permanent("us".into(), up("second.example"))
            .unwrap();

        let (reloaded, _rx2) = store_with(cold_with(&[("default", "cold.example")]), &dir);
        // No history — only the last value survives.
        assert_eq!(reloaded.merged().upstreams["us"].host, "second.example");
        assert_eq!(reloaded.merged().upstreams.len(), 2); // default + us
    }

    #[test]
    fn remove_runtime_falls_back_to_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);

        store
            .apply_permanent("us".into(), up("snap.example"))
            .unwrap();
        store.apply_runtime("us".into(), up("hot.example"));
        assert_eq!(store.diagnose()["us"], Source::Hot);

        assert!(store.remove_runtime("us"));
        // falls back to the snapshot value, not gone
        assert_eq!(store.merged().upstreams["us"].host, "snap.example");
        assert_eq!(store.diagnose()["us"], Source::Snapshot);
    }

    #[test]
    fn diff_surfaces_shadowing_layers() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);
        store.snapshot.insert("default".into(), up("snap.example"));

        let diff = store.diff();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].name, "default");
        assert_eq!(diff[0].effective, Source::Snapshot);
        assert!(diff[0].diverges); // cold host != snapshot host
    }

    #[test]
    fn publish_broadcasts_merged_to_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, rx) = store_with(cold_with(&[("default", "cold.example")]), &dir);

        store.apply_runtime("us".into(), up("us.example"));
        assert!(rx.borrow().upstreams.contains_key("us"));
    }
}
