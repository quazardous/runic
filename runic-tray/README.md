# runic-tray

Windows system-tray front-end over the [`runic`](..) core library — a thin GUI
shell that lets you Start/Stop/Restart the proxy, see status and the current IP,
open the config/logs, from the notification area.

> **Status: scaffold.** This crate was authored from the v1 spec and has **not
> been compiled** in the dev environment (no Windows toolchain there). It is a
> standalone crate — the repo-root `cargo build` / `cargo test` deliberately do
> **not** include it (its GUI deps need Windows + GTK). Build and smoke-test it
> on Windows, then iterate.

## Build (on Windows)

```powershell
cd runic-tray
cargo build --release
.\target\release\runic-tray.exe
```

Prerequisites and the full plan: [`../docs/dev/windows-setup.md`](../docs/dev/windows-setup.md).

## What works vs TODO

- **Wired**: tray menu over a tokio runtime + Supervisor that boots the `runic`
  core (config store, watcher, admin API, SOCKS5 server); Start / Stop / Restart,
  Quit; the Raidho icon recolours live by active route (teal proxied / amber
  direct / blue no-route / grey stopped); native Windows toasts on startup and
  for Status; Show current IP (through the local SOCKS5); Open config file;
  Show logs (`%APPDATA%\runic\runic-tray.log`); **Start with Windows** toggle
  (HKCU Run key); embedded `.ico` (Raidho rune); portable ZIP + MSI packaging.
- **TODO**:
  - Auto-update (#763): check the release feed, verified download, swap on
    restart.
  - Lib follow-up: shutdown handles for the watcher/admin listeners so Stop
    fully tears the core down (today Stop only halts the SOCKS5 server).

## Why a separate crate

The core is the `runic` library; the CLI and this tray are two shells over it.
Keeping all non-UI logic in the library keeps it testable on Linux CI, while the
Windows-only GUI stays isolated here.
