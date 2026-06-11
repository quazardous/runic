//! "Start with Windows" toggle, backed by the HKCU Run key.
//!
//! Writing `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\runic-tray` =
//! the quoted path to this executable makes Windows launch the tray at login;
//! deleting the value disables it. Opt-in only — nothing is written unless the
//! user toggles it on. No extra dependency: raw Win32 via the `windows` crate.

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use anyhow::{bail, Context, Result};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SAM_FLAGS, REG_SZ,
    };

    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "runic-tray";

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// RAII handle so the opened key is always closed.
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    fn open(access: REG_SAM_FLAGS) -> Result<Key> {
        let subkey = wide(RUN_SUBKEY);
        let mut hkey = HKEY::default();
        let rc = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                0,
                access,
                &mut hkey,
            )
        };
        if rc != ERROR_SUCCESS {
            bail!("RegOpenKeyExW(Run) failed: {rc:?}");
        }
        Ok(Key(hkey))
    }

    /// True if the autostart value is present in the Run key.
    pub fn is_enabled() -> bool {
        let Ok(key) = open(KEY_READ) else {
            return false;
        };
        let name = wide(VALUE_NAME);
        let rc = unsafe { RegQueryValueExW(key.0, PCWSTR(name.as_ptr()), None, None, None, None) };
        rc == ERROR_SUCCESS
    }

    /// Enable or disable launch-at-login for the *current* executable. Enabling
    /// (re)writes the value to this exe's path, so it self-heals if the binary
    /// moved.
    pub fn set(enabled: bool) -> Result<()> {
        let key = open(KEY_WRITE)?;
        let name = wide(VALUE_NAME);
        if enabled {
            let exe = std::env::current_exe().context("resolve current_exe")?;
            let value = wide(&format!("\"{}\"", exe.display()));
            // REG_SZ data = the UTF-16 string bytes, including the null terminator.
            let bytes =
                unsafe { std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2) };
            let rc =
                unsafe { RegSetValueExW(key.0, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes)) };
            if rc != ERROR_SUCCESS {
                bail!("RegSetValueExW failed: {rc:?}");
            }
        } else {
            let rc = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
            // Deleting an absent value just means it was already disabled.
            if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
                bail!("RegDeleteValueW failed: {rc:?}");
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use imp::{is_enabled, set};

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn set(_enabled: bool) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::{is_enabled, set};

    /// Exercise the real Run-key round-trip. Non-destructive: only runs when
    /// autostart is currently OFF (the common case) and leaves it OFF, so a
    /// user's real setting is never disturbed.
    #[test]
    fn run_key_round_trip() {
        if is_enabled() {
            return; // don't clobber an existing user setting
        }
        set(true).expect("enable autostart");
        assert!(is_enabled(), "value should be present after enable");
        set(false).expect("disable autostart");
        assert!(!is_enabled(), "value should be gone after disable");
    }
}
