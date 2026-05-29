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

- **Wired**: tray menu, tokio runtime + Supervisor that boots the `runic` core
  (config store, watcher, admin API, SOCKS5 server), Start / Stop / Restart,
  Status (logged), Quit, auto-start of the proxy on launch.
- **TODO** (marked in `src/main.rs`):
  - Show-current-IP via an internal request through the local SOCKS5 + a toast.
  - Open config / Show logs actions.
  - Native Windows toasts for status/IP.
  - Auto-start at login (registry `Run` key / Startup shortcut).
  - A proper `.ico` asset (a placeholder icon is generated for now).
  - Lib follow-up: shutdown handles for the watcher/admin listeners so Stop
    fully tears the core down (today Stop only halts the SOCKS5 server).

## Why a separate crate

The core is the `runic` library; the CLI and this tray are two shells over it.
Keeping all non-UI logic in the library keeps it testable on Linux CI, while the
Windows-only GUI stays isolated here.
