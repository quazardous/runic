//! Boot/rebind announcement: one consolidated log line with both endpoints,
//! plus systemd readiness/status via the sd_notify(3) protocol.
//!
//! The sd_notify part is hand-rolled — a single datagram to `$NOTIFY_SOCKET` —
//! rather than a dependency: we only ever send `READY=1` and `STATUS=…`.
//! It is a silent no-op when the variable is absent (not running under
//! systemd) and on non-Linux platforms (systemd is Linux-only; macOS and
//! Windows service managers have no equivalent datagram contract).
//!
//! Prior art: sd_notify(3) / libsystemd's NOTIFY_SOCKET protocol.

use std::net::SocketAddr;
use std::sync::OnceLock;

use tracing::info;

/// The admin listener's address, published once by `main` after the admin
/// surface is up. `None` in shells that don't run the admin plane (tests,
/// tray with admin disabled) — the announcement then names the SOCKS side only.
static ADMIN_ADDR: OnceLock<SocketAddr> = OnceLock::new();

/// Record the admin address for subsequent announcements. First write wins
/// (the admin address is boot-time-only by design).
pub fn set_admin_addr(addr: SocketAddr) {
    let _ = ADMIN_ADDR.set(addr);
}

/// Announce the (re)bound SOCKS5 endpoint: one consolidated INFO line with
/// both endpoints — greppable in one shot, unlike the per-module bind lines —
/// and, under systemd, `READY=1` + a live `STATUS=` line for `systemctl
/// status`. Called by the server task after every successful (re)bind, so the
/// status line stays true in auto-port mode where a rebind mints a new port.
pub fn socks_bound(socks: SocketAddr) {
    let status = match ADMIN_ADDR.get() {
        Some(admin) => {
            info!(socks5 = %socks, admin = %admin, "runic up");
            format!("STATUS=SOCKS5 {socks} · admin http://{admin}")
        }
        None => {
            info!(socks5 = %socks, "runic up");
            format!("STATUS=SOCKS5 {socks}")
        }
    };
    // READY=1 is idempotent for systemd; sending it on rebinds too keeps this
    // a single chokepoint. Under Type=notify the first one flips the unit
    // from "activating" to "active" — i.e. "active" means "really bound".
    sd_notify(&format!("READY=1\n{status}"));
}

/// Send one message over the sd_notify(3) datagram protocol. No-op when
/// `$NOTIFY_SOCKET` is unset or on non-Linux. Failures are deliberately
/// swallowed: readiness notification must never take the data plane down.
#[cfg(target_os = "linux")]
fn sd_notify(msg: &str) {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr as UnixSocketAddr, UnixDatagram};

    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    // A leading '@' names an abstract socket (systemd uses both forms).
    let sent = match path.strip_prefix('@') {
        Some(abstract_name) => UnixSocketAddr::from_abstract_name(abstract_name.as_bytes())
            .and_then(|addr| sock.send_to_addr(msg.as_bytes(), &addr)),
        None => sock.send_to(msg.as_bytes(), &path),
    };
    if let Err(e) = sent {
        tracing::debug!(error = %e, "sd_notify send failed (ignored)");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_notify(_msg: &str) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// End-to-end over the real protocol: bind a datagram socket, point
    /// `$NOTIFY_SOCKET` at it, announce, and read what systemd would read.
    /// Single test (env vars are process-global; splitting this would race).
    #[test]
    fn socks_bound_sends_ready_and_both_endpoints() {
        use std::os::unix::net::UnixDatagram;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("notify.sock");
        let receiver = UnixDatagram::bind(&sock_path).expect("bind fake NOTIFY_SOCKET");
        std::env::set_var("NOTIFY_SOCKET", &sock_path);

        set_admin_addr("127.0.0.1:48484".parse().unwrap());
        socks_bound("127.0.0.1:41475".parse().unwrap());

        let mut buf = [0u8; 512];
        let n = receiver.recv(&mut buf).expect("datagram received");
        let msg = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(msg.contains("READY=1"), "got: {msg}");
        assert!(msg.contains("STATUS=SOCKS5 127.0.0.1:41475"), "got: {msg}");
        assert!(msg.contains("admin http://127.0.0.1:48484"), "got: {msg}");

        // Rebind: a fresh STATUS with the new port (READY again is fine —
        // idempotent for systemd).
        socks_bound("127.0.0.1:41999".parse().unwrap());
        let n = receiver.recv(&mut buf).expect("second datagram");
        let msg = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(msg.contains("STATUS=SOCKS5 127.0.0.1:41999"), "got: {msg}");

        // Unset → silent no-op (nothing to assert beyond "doesn't panic/hang").
        std::env::remove_var("NOTIFY_SOCKET");
        socks_bound("127.0.0.1:41475".parse().unwrap());
    }
}
