//! Encrypted config silo — the binding-agnostic core.
//!
//! A **silo** is a directory holding **N co-living variations**. Each variation
//! is an independently encrypted snapshot of the config, opened by a per-variation
//! **token** the silo mints once and then forgets.
//!
//! Security model (residential box, foreign zone — see ticket discussion):
//! - **token** = the per-variation secret. 256-bit, machine-minted. It is the
//!   only thing that decrypts a variation. It lives **off the box** (held by the
//!   client/skynet), in runic's RAM only for the duration of an operation.
//! - On disk runic keeps only **`SHA256(token)`** (the variation id / verifier)
//!   and the **ciphertext** — never a raw token. A seized powered-off box yields
//!   nothing usable.
//! - The AEAD key is **`HKDF-SHA256(token)`** — a *distinct* one-way derivation
//!   from the on-disk id, so neither reveals the other.
//!
//! This module is **binding-agnostic**: how a client selects its variation
//! (token in the SOCKS5 auth vs a dedicated loopback port) is a layer above,
//! configured per silo. Nothing here depends on it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::config::Upstream;

const TOKEN_LEN: usize = 32; // 256-bit
const NONCE_LEN: usize = 24; // XChaCha20 nonce
const KEY_LEN: usize = 32;
const HKDF_INFO: &[u8] = b"runic-silo-snapshot-v1";

/// On-disk index schema version.
pub const SILO_INDEX_VERSION: u32 = 1;

// --- Crypto primitives ------------------------------------------------------

/// Mint a fresh 256-bit token from the OS CSPRNG.
fn mint_token() -> Result<[u8; TOKEN_LEN]> {
    let mut t = [0u8; TOKEN_LEN];
    getrandom::getrandom(&mut t).map_err(|e| anyhow!("CSPRNG token: {e}"))?;
    Ok(t)
}

/// On-disk variation id / verifier = `SHA256(token)`, hex. One-way: cannot be
/// reversed to the token, and is useless for decryption.
fn variation_id(token: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(token);
    hex::encode(h.finalize())
}

/// AEAD key = `HKDF-SHA256(token, info)`. A *distinct* one-way function from
/// [`variation_id`], so the on-disk id never yields the key.
fn derive_key(token: &[u8]) -> [u8; KEY_LEN] {
    let hk = Hkdf::<Sha256>::new(None, token);
    let mut key = [0u8; KEY_LEN];
    hk.expand(HKDF_INFO, &mut key)
        .expect("HKDF expand of 32 bytes never fails");
    key
}

/// Encrypt `plaintext` under `token` → `(nonce, ciphertext)`. Fresh random nonce
/// per call (XChaCha's 192-bit nonce makes reuse a non-issue).
fn seal(token: &[u8], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut key = derive_key(token);
    let cipher = XChaCha20Poly1305::new_from_slice(&key).expect("32-byte key");
    key.zeroize();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes).map_err(|e| anyhow!("CSPRNG nonce: {e}"))?;
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|_| anyhow!("seal failed"))?;
    Ok((nonce_bytes.to_vec(), ct))
}

/// Decrypt under `token` + `nonce`. Errors on a wrong token or any tampering
/// (AEAD auth tag) — never returns partial or garbage plaintext.
fn open(token: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if nonce.len() != NONCE_LEN {
        return Err(anyhow!("bad nonce length"));
    }
    let mut key = derive_key(token);
    let cipher = XChaCha20Poly1305::new_from_slice(&key).expect("32-byte key");
    key.zeroize();
    cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| anyhow!("decrypt failed (wrong token or tampered ciphertext)"))
}

/// Encode a raw token for transport (returned once at create).
fn encode_token(token: &[u8]) -> String {
    B64.encode(token)
}

/// Decode a token presented by a caller.
fn decode_token(s: &str) -> Result<Vec<u8>> {
    B64.decode(s)
        .map_err(|e| anyhow!("bad token encoding: {e}"))
}

// --- On-disk model ----------------------------------------------------------

/// The plaintext a variation holds: the same upstream-pool shape as the
/// (cleartext) non-silo snapshot, just encrypted at rest.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariationData {
    #[serde(default)]
    pub upstreams: BTreeMap<String, Upstream>,
}

/// One row of the **cleartext** index. Holds no token and no config plaintext —
/// only the hash (one-way), public timestamps, and the public AEAD nonce — so the
/// TTL GC can run without any token.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    created_at: u64,
    last_access: u64,
    nonce: String, // base64url of the blob's nonce
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    version: u32,
    variations: BTreeMap<String, IndexEntry>, // variation_id -> entry
}

/// A filesystem-backed silo: a cleartext `index.json` + one encrypted `<id>.snap`
/// blob per variation. Single-threaded by construction (callers serialize).
pub struct SiloStore {
    dir: PathBuf,
    index: Index,
    ttl_secs: u64,
}

impl SiloStore {
    /// Open (creating the directory if needed) and load the index.
    pub fn open(dir: PathBuf, ttl_secs: u64) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create silo dir {}", dir.display()))?;
        let index = load_index(&dir.join("index.json"))?;
        Ok(Self {
            dir,
            index,
            ttl_secs,
        })
    }

    pub fn len(&self) -> usize {
        self.index.variations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.variations.is_empty()
    }

    /// Create a new variation: mint a token, encrypt an empty snapshot, write the
    /// blob + index entry, and return the token **once**. The raw token is wiped
    /// from runic's memory before returning (only the caller keeps it).
    pub fn create_variation(&mut self, now: u64) -> Result<String> {
        let mut token = mint_token()?;
        let id = variation_id(&token);
        let pt = serde_json::to_vec(&VariationData::default())?;
        let (nonce, ct) = seal(&token, &pt)?;
        write_atomic(&self.blob_path(&id), &ct)?;
        self.index.variations.insert(
            id,
            IndexEntry {
                created_at: now,
                last_access: now,
                nonce: B64.encode(&nonce),
            },
        );
        self.persist_index()?;
        let encoded = encode_token(&token);
        token.zeroize();
        Ok(encoded)
    }

    /// Open a variation by token: decrypt its blob into RAM and bump last-access.
    pub fn open_variation(&mut self, token_b64: &str, now: u64) -> Result<VariationData> {
        let mut token = decode_token(token_b64)?;
        let id = variation_id(&token);
        let entry = self
            .index
            .variations
            .get(&id)
            .ok_or_else(|| anyhow!("no such variation"))?
            .clone();
        let nonce = B64.decode(&entry.nonce).context("decode nonce")?;
        let ct = std::fs::read(self.blob_path(&id)).context("read variation blob")?;
        let pt = open(&token, &nonce, &ct);
        token.zeroize();
        let data: VariationData = serde_json::from_slice(&pt?).context("parse variation")?;
        if let Some(e) = self.index.variations.get_mut(&id) {
            e.last_access = now;
        }
        self.persist_index()?;
        Ok(data)
    }

    /// Replace a variation's data, re-encrypting with a fresh nonce.
    pub fn write_variation(
        &mut self,
        token_b64: &str,
        data: &VariationData,
        now: u64,
    ) -> Result<()> {
        let mut token = decode_token(token_b64)?;
        let id = variation_id(&token);
        if !self.index.variations.contains_key(&id) {
            token.zeroize();
            return Err(anyhow!("no such variation"));
        }
        let pt = serde_json::to_vec(data)?;
        let sealed = seal(&token, &pt);
        token.zeroize();
        let (nonce, ct) = sealed?;
        write_atomic(&self.blob_path(&id), &ct)?;
        if let Some(e) = self.index.variations.get_mut(&id) {
            e.nonce = B64.encode(&nonce);
            e.last_access = now;
        }
        self.persist_index()?;
        Ok(())
    }

    /// Purge variations idle past the TTL. Runs **without any token** (operates on
    /// the cleartext index only). Returns the number purged.
    pub fn gc(&mut self, now: u64) -> Result<usize> {
        self.gc_except(now, &std::collections::HashSet::new())
    }

    /// Like [`Self::gc`] but never purges a variation whose id is in `protected`
    /// (e.g. currently warm in a [`VariationCache`] — in active use).
    pub fn gc_except(
        &mut self,
        now: u64,
        protected: &std::collections::HashSet<String>,
    ) -> Result<usize> {
        let expired: Vec<String> = self
            .index
            .variations
            .iter()
            .filter(|(id, e)| {
                !protected.contains(*id) && now.saturating_sub(e.last_access) > self.ttl_secs
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            let _ = std::fs::remove_file(self.blob_path(id));
            self.index.variations.remove(id);
        }
        if !expired.is_empty() {
            self.persist_index()?;
        }
        Ok(expired.len())
    }

    /// Stamp a known variation's last-access (no token needed — cleartext index).
    /// Used when a warm cache entry goes cold, so the disk TTL counts from then.
    pub fn touch_id(&mut self, id: &str, now: u64) -> Result<()> {
        if let Some(e) = self.index.variations.get_mut(id) {
            e.last_access = now;
            self.persist_index()?;
        }
        Ok(())
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.snap"))
    }

    fn persist_index(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.index)?;
        write_atomic(&self.dir.join("index.json"), &bytes)
    }
}

fn load_index(path: &Path) -> Result<Index> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let idx: Index = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse silo index {}", path.display()))?;
            Ok(idx)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index {
            version: SILO_INDEX_VERSION,
            variations: BTreeMap::new(),
        }),
        Err(e) => Err(e).with_context(|| format!("read silo index {}", path.display())),
    }
}

/// Atomic-replace write (tempfile + rename), owner-only (`0600`) on unix. A crash
/// mid-write leaves the previous file intact — the rename is the commit point.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("open temp {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Idle-evicting RAM cache of decrypted variations over a [`SiloStore`] — the
/// "keep-alive" layer. A variation is decrypted on first access and kept warm
/// while in use; once idle past `idle_ttl_secs` it is dropped from RAM (its
/// plaintext config no longer resides in memory) and its on-disk last-access is
/// stamped so the disk decay TTL counts from when it went cold.
pub struct VariationCache {
    store: SiloStore,
    warm: HashMap<String, WarmEntry>,
    idle_ttl_secs: u64,
}

struct WarmEntry {
    data: VariationData,
    last_touch: u64,
}

impl VariationCache {
    pub fn new(store: SiloStore, idle_ttl_secs: u64) -> Self {
        Self {
            store,
            warm: HashMap::new(),
            idle_ttl_secs,
        }
    }

    /// Create a new variation; returns its token once (delegates to the store).
    pub fn create(&mut self, now: u64) -> Result<String> {
        self.store.create_variation(now)
    }

    /// Access a variation by token. A warm-cache hit returns the cached config
    /// (no disk, no re-decrypt); a miss decrypts from disk and warms it.
    pub fn access(&mut self, token: &str, now: u64) -> Result<VariationData> {
        let id = variation_id(&decode_token(token)?);
        if let Some(e) = self.warm.get_mut(&id) {
            e.last_touch = now;
            return Ok(e.data.clone());
        }
        let data = self.store.open_variation(token, now)?;
        self.warm.insert(
            id,
            WarmEntry {
                data: data.clone(),
                last_touch: now,
            },
        );
        Ok(data)
    }

    /// Persist a variation's data (re-encrypt) and refresh its warm copy.
    pub fn write(&mut self, token: &str, data: &VariationData, now: u64) -> Result<()> {
        self.store.write_variation(token, data, now)?;
        let id = variation_id(&decode_token(token)?);
        self.warm.insert(
            id,
            WarmEntry {
                data: data.clone(),
                last_touch: now,
            },
        );
        Ok(())
    }

    /// Drop variations idle past `idle_ttl_secs` from RAM, stamping their on-disk
    /// last-access so the disk decay TTL counts from cold. Returns how many evicted.
    pub fn evict_idle(&mut self, now: u64) -> Result<usize> {
        let cold: Vec<String> = self
            .warm
            .iter()
            .filter(|(_, e)| now.saturating_sub(e.last_touch) > self.idle_ttl_secs)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &cold {
            self.warm.remove(id);
            self.store.touch_id(id, now)?;
        }
        Ok(cold.len())
    }

    /// Disk decay GC that never purges a variation currently warm in RAM.
    pub fn gc(&mut self, now: u64) -> Result<usize> {
        let protected: HashSet<String> = self.warm.keys().cloned().collect();
        self.store.gc_except(now, &protected)
    }

    /// One maintenance pass: evict idle warm entries, then run the disk decay GC.
    /// Returns `(evicted, purged)`. This is what the background sweeper calls.
    pub fn sweep(&mut self, now: u64) -> Result<(usize, usize)> {
        let evicted = self.evict_idle(now)?;
        let purged = self.gc(now)?;
        Ok((evicted, purged))
    }

    pub fn warm_len(&self) -> usize {
        self.warm.len()
    }

    pub fn store(&self) -> &SiloStore {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok() -> [u8; TOKEN_LEN] {
        mint_token().unwrap()
    }

    // --- crypto primitives --------------------------------------------------

    #[test]
    fn seal_open_round_trips() {
        let t = tok();
        let (nonce, ct) = seal(&t, b"hello world").unwrap();
        let pt = open(&t, &nonce, &ct).unwrap();
        assert_eq!(pt, b"hello world");
    }

    #[test]
    fn open_with_wrong_token_fails() {
        let (nonce, ct) = seal(&tok(), b"secret").unwrap();
        let other = tok();
        assert!(open(&other, &nonce, &ct).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let t = tok();
        let (nonce, mut ct) = seal(&t, b"secret payload").unwrap();
        ct[0] ^= 0x01; // flip one bit
        assert!(open(&t, &nonce, &ct).is_err());
    }

    #[test]
    fn tampered_nonce_is_rejected() {
        let t = tok();
        let (mut nonce, ct) = seal(&t, b"secret payload").unwrap();
        nonce[0] ^= 0x01;
        assert!(open(&t, &nonce, &ct).is_err());
    }

    #[test]
    fn id_is_stable_and_distinct() {
        let a = tok();
        let b = tok();
        assert_eq!(variation_id(&a), variation_id(&a));
        assert_ne!(variation_id(&a), variation_id(&b));
    }

    #[test]
    fn key_is_not_the_on_disk_id() {
        // The on-disk verifier (SHA256) must not equal/derive the AEAD key (HKDF),
        // otherwise a seized disk could decrypt.
        let t = tok();
        let id = variation_id(&t); // hex of SHA256
        let key = derive_key(&t);
        assert_ne!(hex::encode(key), id);
    }

    #[test]
    fn minted_tokens_do_not_collide() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(ids.insert(variation_id(&tok())), "token id collision");
        }
    }

    // --- store --------------------------------------------------------------

    fn store(ttl: u64) -> (SiloStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = SiloStore::open(dir.path().join("runic.silo"), ttl).unwrap();
        (s, dir)
    }

    fn upstream_data(host: &str, user: &str, pass: &str) -> VariationData {
        let mut upstreams = BTreeMap::new();
        upstreams.insert(
            "default".to_string(),
            Upstream {
                kind: crate::config::UpstreamKind::HttpConnect,
                host: host.to_string(),
                port: 823,
                auth: crate::config::UpstreamCreds {
                    username: user.to_string(),
                    password: pass.to_string(),
                },
            },
        );
        VariationData { upstreams }
    }

    fn all_silo_bytes(dir: &Path) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                out.extend_from_slice(&std::fs::read(&p).unwrap());
            }
        }
        out
    }

    #[test]
    fn create_then_open_round_trips_via_store() {
        let (mut s, _d) = store(3600);
        let token = s.create_variation(0).unwrap();
        assert_eq!(s.len(), 1);
        let data = s.open_variation(&token, 1).unwrap();
        assert_eq!(data, VariationData::default());
    }

    #[test]
    fn mint_and_forget_leaves_no_raw_token_on_disk() {
        let (mut s, _d) = store(3600);
        let token = s.create_variation(0).unwrap();
        let raw = decode_token(&token).unwrap();
        let bytes = all_silo_bytes(&s.dir);
        // Neither the raw token bytes nor its base64 form appear anywhere on disk.
        assert!(
            !contains_subslice(&bytes, &raw),
            "raw token bytes found on disk"
        );
        assert!(
            !contains_subslice(&bytes, token.as_bytes()),
            "encoded token found on disk"
        );
    }

    #[test]
    fn variations_are_isolated() {
        let (mut s, _d) = store(3600);
        let ta = s.create_variation(0).unwrap();
        let tb = s.create_variation(0).unwrap();

        s.write_variation(&ta, &upstream_data("a.example", "ua", "pa"), 1)
            .unwrap();
        s.write_variation(&tb, &upstream_data("b.example", "ub", "pb"), 1)
            .unwrap();

        // Each token only opens its own variation's data.
        assert_eq!(
            s.open_variation(&ta, 2).unwrap().upstreams["default"].host,
            "a.example"
        );
        assert_eq!(
            s.open_variation(&tb, 2).unwrap().upstreams["default"].host,
            "b.example"
        );
        // A token cannot be used against the other variation (different id ⇒ not found,
        // and even the blob wouldn't decrypt) — covered by open returning B's own data
        // above, never A's.
    }

    #[test]
    fn headline_no_plaintext_creds_on_disk() {
        // THE security test: after create + write with real creds, scan the whole
        // silo dir — the secret must appear nowhere in the clear.
        let (mut s, _d) = store(3600);
        let token = s.create_variation(0).unwrap();
        s.write_variation(
            &token,
            &upstream_data("gw.example", "the-user", "SUPER-SECRET-PASSWORD"),
            1,
        )
        .unwrap();

        let bytes = all_silo_bytes(&s.dir);
        assert!(
            !contains_subslice(&bytes, b"SUPER-SECRET-PASSWORD"),
            "plaintext password leaked to disk"
        );
        assert!(
            !contains_subslice(&bytes, b"gw.example"),
            "plaintext host leaked to disk"
        );
        assert!(
            !contains_subslice(&bytes, b"the-user"),
            "plaintext username leaked to disk"
        );
    }

    #[test]
    fn gc_purges_idle_but_keeps_active() {
        let (mut s, _d) = store(100); // TTL = 100s
        let idle = s.create_variation(0).unwrap();
        let active = s.create_variation(0).unwrap();

        // At t=50, touch `active` (bumps its last_access).
        s.open_variation(&active, 50).unwrap();

        // At t=200: idle (last_access 0, age 200 > 100) purged; active (age 150 > 100)…
        // also purged unless re-touched. Touch active again at t=200 first.
        s.open_variation(&active, 200).unwrap();
        let purged = s.gc(200).unwrap();
        assert_eq!(purged, 1, "exactly the idle variation should be purged");
        assert!(s.open_variation(&idle, 201).is_err(), "idle should be gone");
        assert!(
            s.open_variation(&active, 201).is_ok(),
            "active should survive"
        );
    }

    #[test]
    fn gc_runs_without_tokens_and_removes_blob() {
        let (mut s, _d) = store(10);
        let token = s.create_variation(0).unwrap();
        let id = variation_id(&decode_token(&token).unwrap());
        assert!(s.blob_path(&id).exists());
        let purged = s.gc(100).unwrap(); // age 100 > 10
        assert_eq!(purged, 1);
        assert!(!s.blob_path(&id).exists(), "blob file should be removed");
    }

    #[test]
    fn index_reloads_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let silo_dir = dir.path().join("runic.silo");
        let token = {
            let mut s = SiloStore::open(silo_dir.clone(), 3600).unwrap();
            let t = s.create_variation(0).unwrap();
            s.write_variation(&t, &upstream_data("persisted.example", "u", "p"), 1)
                .unwrap();
            t
        };
        // Reopen from disk — the variation and its data survive.
        let mut s2 = SiloStore::open(silo_dir, 3600).unwrap();
        assert_eq!(s2.len(), 1);
        assert_eq!(
            s2.open_variation(&token, 2).unwrap().upstreams["default"].host,
            "persisted.example"
        );
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let (mut s, _d) = store(3600);
        s.create_variation(0).unwrap();
        for entry in std::fs::read_dir(&s.dir).unwrap() {
            let p = entry.unwrap().path();
            assert_ne!(
                p.extension().and_then(|e| e.to_str()),
                Some("tmp"),
                "a .tmp file was left behind: {}",
                p.display()
            );
        }
    }

    // --- VariationCache (keep-alive RAM layer) -------------------------------

    fn cache(disk_ttl: u64, idle_ttl: u64) -> (VariationCache, tempfile::TempDir) {
        let (s, d) = store(disk_ttl);
        (VariationCache::new(s, idle_ttl), d)
    }

    #[test]
    fn cache_access_warms_and_round_trips() {
        let (mut c, _d) = cache(3600, 3600);
        let token = c.create(0).unwrap();
        assert_eq!(c.access(&token, 1).unwrap(), VariationData::default());
        c.write(&token, &upstream_data("c.example", "u", "p"), 2)
            .unwrap();
        assert_eq!(
            c.access(&token, 3).unwrap().upstreams["default"].host,
            "c.example"
        );
        assert_eq!(c.warm_len(), 1);
    }

    #[test]
    fn cache_evicts_idle_then_redecrypts() {
        let (mut c, _d) = cache(100_000, 100); // big disk TTL, idle TTL = 100
        let token = c.create(0).unwrap();
        c.write(&token, &upstream_data("warm.example", "u", "p"), 0)
            .unwrap();
        c.access(&token, 0).unwrap();
        assert_eq!(c.warm_len(), 1);

        assert_eq!(c.evict_idle(50).unwrap(), 0, "not idle yet");
        assert_eq!(c.evict_idle(200).unwrap(), 1, "idle past 100s");
        assert_eq!(c.warm_len(), 0);

        // Re-access re-decrypts from disk — data intact.
        assert_eq!(
            c.access(&token, 201).unwrap().upstreams["default"].host,
            "warm.example"
        );
        assert_eq!(c.warm_len(), 1);
    }

    #[test]
    fn cache_gc_protects_warm_variations() {
        let (mut c, _d) = cache(100, 100_000); // disk TTL 100, idle TTL huge
        let cold = c.create(0).unwrap(); // on disk, never warmed
        let warm = c.create(0).unwrap();
        c.access(&warm, 0).unwrap(); // warm it

        // now=200 > disk TTL: the cold variation is purged, the warm one protected.
        assert_eq!(c.gc(200).unwrap(), 1, "only the cold variation is purged");
        assert!(
            c.access(&cold, 201).is_err(),
            "cold variation should be gone"
        );
        assert!(
            c.access(&warm, 201).is_ok(),
            "warm variation should survive"
        );
    }

    #[test]
    fn cache_sweep_evicts_idle_and_purges_expired() {
        let (mut c, _d) = cache(100, 50); // disk TTL 100s, idle TTL 50s
        let a = c.create(0).unwrap();
        c.access(&a, 0).unwrap(); // A is warm
        let b = c.create(0).unwrap(); // B stays cold on disk

        // At t=200: A is idle-evicted from RAM (its disk stamp refreshed to 200,
        // so it survives the disk GC); B (cold, last_access 0) is purged.
        let (evicted, purged) = c.sweep(200).unwrap();
        assert_eq!(evicted, 1, "A evicted from RAM");
        assert_eq!(purged, 1, "B purged from disk");
        assert!(c.access(&a, 201).is_ok(), "A survived (was warm at sweep)");
        assert!(c.access(&b, 201).is_err(), "B was purged");
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || haystack.len() < needle.len() {
            return false;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
