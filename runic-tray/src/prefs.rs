//! Tiny persisted tray preferences under `HKCU\Software\runic\runic-tray`,
//! raw Win32 like [`crate::autostart`] (no extra dependency). Currently just the
//! opt-in "Auto-update" flag (whether to check for updates at startup).

/// True if auto-update-at-startup is enabled (defaults to false / opt-in).
pub fn auto_update_enabled() -> bool {
    #[cfg(windows)]
    {
        imp::get_bool("auto_update")
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Persist the auto-update-at-startup flag.
pub fn set_auto_update(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        imp::set_bool("auto_update", enabled)
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use anyhow::{bail, Result};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        REG_DWORD, RRF_RT_REG_DWORD,
    };

    const SUBKEY: &str = r"Software\runic\runic-tray";

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn get_bool(name: &str) -> bool {
        let sub = wide(SUBKEY);
        let n = wide(name);
        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                PCWSTR(n.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut data as *mut u32 as *mut std::ffi::c_void),
                Some(&mut size),
            )
        };
        rc == ERROR_SUCCESS && data != 0
    }

    pub fn set_bool(name: &str, value: bool) -> Result<()> {
        let sub = wide(SUBKEY);
        let mut hkey = HKEY::default();
        let rc = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), &mut hkey) };
        if rc != ERROR_SUCCESS {
            bail!("RegCreateKeyW(runic-tray prefs) failed: {rc:?}");
        }
        let n = wide(name);
        let data: u32 = value.into();
        let bytes = data.to_ne_bytes();
        let rc = unsafe { RegSetValueExW(hkey, PCWSTR(n.as_ptr()), 0, REG_DWORD, Some(&bytes)) };
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        if rc != ERROR_SUCCESS {
            bail!("RegSetValueExW({name}) failed: {rc:?}");
        }
        Ok(())
    }
}
