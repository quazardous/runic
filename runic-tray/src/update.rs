//! Auto-update (#763): check the GitHub Releases feed for a newer tray build
//! and apply it through the verified MSI.
//!
//! Tray releases are tagged `tray-vX.Y.Z` (the core/lib/CLI uses a bare
//! `vX.Y.Z`), so we filter on that prefix. Updates are applied via the MSI
//! (#764) whose frozen UpgradeCode makes Windows Installer upgrade in place — no
//! fragile running-exe swap. The artifact's SHA256 is verified against the
//! release's `.sha256` sidecar BEFORE msiexec ever runs it: this is a proxy
//! tool, so it must never execute an unverified binary.
//!
//! Network calls here are blocking (ureq) — run them off the UI thread (the tray
//! drives them on the tokio runtime via `spawn_blocking`).

use anyhow::{anyhow, bail, Context, Result};

const RELEASES_API: &str = "https://api.github.com/repos/quazardous/runic/releases?per_page=30";
const TAG_PREFIX: &str = "tray-v";
const UA: &str = "runic-tray";

/// A tray release newer than the running build.
#[derive(Clone, Debug)]
pub struct Update {
    /// Semantic version without the tag prefix, e.g. "0.2.0".
    pub version: String,
    /// `browser_download_url` of the `.msi` asset.
    pub msi_url: String,
    /// Expected lowercase-hex SHA256, read from the `.msi.sha256` sidecar asset.
    pub sha256: String,
}

/// The version baked into this build ("0.1.0").
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Parse "1.2.3" → (1, 2, 3); missing/garbage parts default to 0.
fn semver(v: &str) -> (u64, u64, u64) {
    let mut it = v.split('.').map(|p| p.trim().parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Query the releases feed and return the newest tray release strictly newer
/// than the running version, or `None` if already up to date.
pub fn check() -> Result<Option<Update>> {
    let body: serde_json::Value = ureq::get(RELEASES_API)
        .set("User-Agent", UA)
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("GitHub releases request failed")?
        .into_json()
        .context("parse releases JSON")?;
    let releases = body
        .as_array()
        .ok_or_else(|| anyhow!("releases response is not an array"))?;

    let cur = semver(current());
    let mut best: Option<((u64, u64, u64), Update)> = None;

    for rel in releases {
        let truthy = |k: &str| rel.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
        if truthy("draft") || truthy("prerelease") {
            continue;
        }
        let tag = rel.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
        let Some(ver_str) = tag.strip_prefix(TAG_PREFIX) else {
            continue;
        };
        let ver = semver(ver_str);
        if ver <= cur {
            continue;
        }
        let Some(assets) = rel.get("assets").and_then(|a| a.as_array()) else {
            continue;
        };
        let url_of = |suffix: &str| -> Option<String> {
            assets
                .iter()
                .find(|a| {
                    a.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| n.ends_with(suffix))
                        .unwrap_or(false)
                })
                .and_then(|a| a.get("browser_download_url"))
                .and_then(|u| u.as_str())
                .map(str::to_string)
        };
        // The .sha256 check must come before the .msi match, else ".msi" also
        // matches ".msi.sha256".
        let (Some(sha_url), Some(msi_url)) = (url_of(".msi.sha256"), url_of(".msi")) else {
            continue;
        };

        // Pull the small sidecar now: "<hash>  <filename>".
        let sha_text = ureq::get(&sha_url)
            .set("User-Agent", UA)
            .call()
            .context("download sha256 sidecar")?
            .into_string()
            .context("read sha256 sidecar")?;
        let expected = sha_text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        if expected.len() != 64 {
            continue;
        }

        if best.as_ref().map(|(bv, _)| ver > *bv).unwrap_or(true) {
            best = Some((
                ver,
                Update {
                    version: ver_str.to_string(),
                    msi_url,
                    sha256: expected,
                },
            ));
        }
    }
    Ok(best.map(|(_, u)| u))
}

/// Download the MSI, verify its SHA256, write it to a temp file, and launch
/// msiexec to upgrade in place. On success the caller MUST exit shortly after so
/// the running exe is unlocked for the installer to replace.
pub fn apply(update: &Update) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut bytes = Vec::new();
    ureq::get(&update.msi_url)
        .set("User-Agent", UA)
        .call()
        .context("download MSI")?
        .into_reader()
        .read_to_end(&mut bytes)
        .context("read MSI body")?;

    // Verify BEFORE writing/running anything executable.
    let got = hex(&Sha256::digest(&bytes));
    if got != update.sha256 {
        bail!(
            "checksum mismatch — refusing to install (expected {}, got {got})",
            update.sha256
        );
    }

    let msi_path =
        std::env::temp_dir().join(format!("runic-tray-{}-windows-x64.msi", update.version));
    std::fs::write(&msi_path, &bytes).context("write MSI to temp")?;

    // /i = install-or-upgrade (MajorUpgrade by UpgradeCode), /qb = basic UI with
    // a progress bar. perMachine → msiexec elevates via UAC.
    std::process::Command::new("msiexec")
        .args(["/i", &msi_path.to_string_lossy(), "/qb"])
        .spawn()
        .context("launch msiexec")?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
