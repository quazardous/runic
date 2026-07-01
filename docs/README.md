# runic — documentation

Install guides per OS:

- [`install/docker.md`](install/docker.md) — Docker sidecar (OS-agnostic, what
  the root `README.md` quick-starts you with).
- [`install/linux-systemd.md`](install/linux-systemd.md) — Linux: `.deb`/`.rpm`
  package (system service), prebuilt binary, or build from source (per-user
  systemd unit). Hot-reload on config change.

Feature guides:

- [`install/socks5-routing.md`](install/socks5-routing.md) — Multi-provider
  routing via the SOCKS5 username (v0.7+).
- [`install/admin-api.md`](install/admin-api.md) — Admin API for runtime /
  permanent config changes, firewalld-style (v0.6+).
- [`install/filtering.md`](install/filtering.md) — Domain filtering: allow/deny
  the target host at CONNECT (bandwidth savings, ad/CDN blocking) — no MITM,
  global and per-silo.
- [`install/silo.md`](install/silo.md) — Config silo: encrypted-at-rest,
  per-client config whose keys never touch the box's disk (opt-in).

Development:

- [`dev/windows-setup.md`](dev/windows-setup.md) — set up a Windows workstation
  to build runic and the planned `runic-tray` app (v1 Windows system tray).

The Windows system-tray app is planned for v1 (Linux uses the systemd path).

For the project overview, scope and design notes, see the root
[`README.md`](../README.md).
