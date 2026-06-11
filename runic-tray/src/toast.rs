//! Native Windows toast notifications, built directly on the `windows` crate
//! already pulled in for the registry work — no extra dependency.
//!
//! Branding: runic registers its OWN AppUserModelID (AUMID) so toasts read
//! "runic" with the rune icon, instead of borrowing the Windows PowerShell
//! AUMID (which made them show up as "Windows PowerShell"). For an unpackaged
//! desktop app, Windows 10 1709+ / 11 reads the toast's display name + icon
//! from `HKCU\Software\Classes\AppUserModelId\<AUMID>` — no MSIX identity or
//! Start-menu shortcut needed. `init()` writes that key + pins the AUMID to the
//! process; call it once at startup before showing any toast.
//!
//! `toast()` must be called from the main / UI thread (the tao event loop),
//! whose COM apartment tao has already initialised — network work that feeds a
//! toast (e.g. Show IP) runs on the tokio runtime and marshals the final string
//! back to the loop via the `EventLoopProxy`.

/// Register runic's AUMID (display name + icon) and pin it to this process so
/// toasts are attributed to "runic". Best-effort: a failure only means toasts
/// fall back to a blank attribution, never a crash. Call once at startup.
#[cfg(windows)]
pub fn init() {
    if let Err(e) = imp::init() {
        tracing::warn!(error = %e, "toast AUMID registration failed");
    }
}

#[cfg(not(windows))]
pub fn init() {}

/// Show a two-line toast (bold `title` + `body`). Best-effort: any failure is
/// logged, never propagated — a missing toast must not take the tray down.
#[cfg(windows)]
pub fn toast(title: &str, body: &str) {
    if let Err(e) = imp::show(title, body) {
        tracing::warn!(error = %e, "toast failed");
    }
}

#[cfg(not(windows))]
pub fn toast(title: &str, body: &str) {
    tracing::info!(%title, %body, "toast");
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;

    use anyhow::{bail, Result};
    use windows::core::{HSTRING, PCWSTR};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, REG_SZ,
    };
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows::UI::Notifications::{
        ToastNotification, ToastNotificationManager, ToastTemplateType,
    };

    // Stable AppUserModelID — Company.Product.SubProduct form, never changes.
    const AUMID: &str = "Quazardous.Runic.Tray";

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Materialise the embedded rune icon next to the config/log files so the
    /// AUMID can point a stable file path at it (toast attribution icon).
    fn ensure_icon() -> Option<PathBuf> {
        const ICON: &[u8] = include_bytes!("../assets/runic.ico");
        let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("runic");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("tray.ico");
        if !path.exists() {
            std::fs::write(&path, ICON).ok()?;
        }
        Some(path)
    }

    unsafe fn set_sz(hkey: HKEY, name: &str, value: &str) -> Result<()> {
        let name_w = wide(name);
        let value_w = wide(value);
        // REG_SZ data = the UTF-16 string bytes, including the null terminator.
        let bytes = std::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2);
        let rc = RegSetValueExW(hkey, PCWSTR(name_w.as_ptr()), 0, REG_SZ, Some(bytes));
        if rc != ERROR_SUCCESS {
            bail!("RegSetValueExW({name}) failed: {rc:?}");
        }
        Ok(())
    }

    pub fn init() -> Result<()> {
        // 1. HKCU\Software\Classes\AppUserModelId\<AUMID> = { DisplayName, IconUri }
        let subkey = wide(&format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
        let mut hkey = HKEY::default();
        // Creates the key (or opens it if present) with default security — enough
        // to write our two string values under HKCU.
        let rc = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr()), &mut hkey) };
        if rc != ERROR_SUCCESS {
            bail!("RegCreateKeyW(AppUserModelId) failed: {rc:?}");
        }
        let written = (|| unsafe {
            set_sz(hkey, "DisplayName", "runic")?;
            if let Some(icon) = ensure_icon() {
                set_sz(hkey, "IconUri", &icon.to_string_lossy())?;
            }
            Ok::<(), anyhow::Error>(())
        })();
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        written?;

        // 2. Pin the AUMID to this process so its toasts use the key above.
        let aumid_w = wide(AUMID);
        unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(aumid_w.as_ptr()))? };
        Ok(())
    }

    pub fn show(title: &str, body: &str) -> Result<()> {
        // ToastText02 = one bold heading line + one wrapped body line.
        let xml: XmlDocument =
            ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)?;
        let texts = xml.GetElementsByTagName(&HSTRING::from("text"))?;
        texts
            .Item(0)?
            .AppendChild(&xml.CreateTextNode(&HSTRING::from(title))?)?;
        texts
            .Item(1)?
            .AppendChild(&xml.CreateTextNode(&HSTRING::from(body))?)?;

        let toast = ToastNotification::CreateToastNotification(&xml)?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
        notifier.Show(&toast)?;
        Ok(())
    }
}
