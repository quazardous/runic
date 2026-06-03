//! Platform-aware default locations for runic's config and persisted state.
//!
//! A single source of truth so the CLI binary (`runic`) and the Windows tray
//! front-end (`runic-tray`) never diverge on where `runic.yaml` and the snapshot
//! live. The tray used to carry its own `%APPDATA%` resolver; it now calls in
//! here instead.
//!
//! Layout:
//!
//! | Path                     | Windows                          | Unix                                   |
//! |--------------------------|----------------------------------|----------------------------------------|
//! | [`config_dir`]           | `%APPDATA%\runic`                | `$XDG_CONFIG_HOME/runic` → `$HOME/.config/runic` |
//! | [`default_config_path`]  | `%APPDATA%\runic\runic.yaml`     | `/etc/runic/runic.yaml` (system path)  |
//!
//! Note the asymmetry on Unix: the CLI config default stays the **system** path
//! `/etc/runic/runic.yaml` (Docker mounts there, the systemd unit references it),
//! while user-state (the snapshot, the silo) lives under [`config_dir`]. On
//! Windows both collapse to the per-user `%APPDATA%\runic` tree, which NTFS
//! already restricts to the current user by inheritance.

use std::path::PathBuf;

/// Per-user config/state directory for runic.
///
/// - Windows: `%APPDATA%\runic` (the roaming profile — ACL-restricted to the
///   current user by NTFS inheritance), falling back to `.\runic` if `APPDATA`
///   is somehow unset.
/// - Unix: `$XDG_CONFIG_HOME/runic`, else `$HOME/.config/runic`, else `./runic`.
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        win_config_dir(std::env::var_os("APPDATA"))
    }
    #[cfg(not(windows))]
    {
        unix_config_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
    }
}

/// Default path the CLI's `--config` falls back to when none is given.
///
/// - Windows: `%APPDATA%\runic\runic.yaml` (same tree as the rest of user-state).
/// - Unix: the **system** path `/etc/runic/runic.yaml`, unchanged — Docker mounts
///   the config there and the systemd unit points at it.
pub fn default_config_path() -> PathBuf {
    #[cfg(windows)]
    {
        config_dir().join("runic.yaml")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/etc/runic/runic.yaml")
    }
}

#[cfg(not(windows))]
fn unix_config_dir(
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    xdg_config_home
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("runic")
}

#[cfg(windows)]
fn win_config_dir(appdata: Option<std::ffi::OsString>) -> PathBuf {
    appdata
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("runic")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn unix_prefers_xdg_config_home() {
        let dir = unix_config_dir(Some("/x/cfg".into()), Some("/home/u".into()));
        assert_eq!(dir, PathBuf::from("/x/cfg/runic"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_falls_back_to_home_dotconfig() {
        let dir = unix_config_dir(None, Some("/home/u".into()));
        assert_eq!(dir, PathBuf::from("/home/u/.config/runic"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_falls_back_to_cwd_when_nothing_set() {
        let dir = unix_config_dir(None, None);
        assert_eq!(dir, PathBuf::from("./runic"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_config_default_is_the_system_path() {
        assert_eq!(
            default_config_path(),
            PathBuf::from("/etc/runic/runic.yaml")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_uses_appdata() {
        let dir = win_config_dir(Some(r"C:\Users\u\AppData\Roaming".into()));
        assert_eq!(dir, PathBuf::from(r"C:\Users\u\AppData\Roaming\runic"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_falls_back_to_cwd_when_appdata_unset() {
        let dir = win_config_dir(None);
        assert_eq!(dir, PathBuf::from(r".\runic"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_config_default_is_under_appdata() {
        assert!(default_config_path().ends_with("runic.yaml"));
    }
}
