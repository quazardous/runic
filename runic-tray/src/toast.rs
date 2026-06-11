//! Native Windows toast notifications, built directly on the `windows` crate
//! already pulled in for the registry work — no extra dependency.
//!
//! Toasts are posted under the built-in Windows PowerShell AppUserModelID so
//! they show WITHOUT the app first registering its own Start-menu shortcut /
//! AUMID. A proper "runic"-branded AUMID (own name + icon on the toast) comes
//! with the MSI installer (#764).
//!
//! `toast()` must be called from the main / UI thread (the tao event loop),
//! whose COM apartment tao has already initialised — network work that feeds a
//! toast (e.g. Show IP) runs on the tokio runtime and marshals the final string
//! back to the loop via the `EventLoopProxy`.

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
    use anyhow::Result;
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{
        ToastNotification, ToastNotificationManager, ToastTemplateType,
    };

    // Built-in PowerShell AUMID — lets toasts appear before runic registers its
    // own (the installer's job, #764). Same trick the winrt-notification crates use.
    const AUMID: &str =
        "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";

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
