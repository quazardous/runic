# Windows dev environment — building runic + the tray app

How to set up a Windows workstation to build and develop runic, and the plan
for the `runic-tray` app (the v1 Windows system-tray front-end). Linux users do
**not** need any of this — they run runic via the CLI / systemd unit.

> Status: the core has been split into a reusable library (the `runic` `[lib]`
> target) so a tray front-end can link against it. The tray crate itself
> (`runic-tray`) is built on Windows — this doc is what gets you there.

## 1. Toolchain

runic uses the **MSVC** toolchain on Windows (the default, and what the tray
GUI crates expect).

1. **Visual Studio Build Tools** — install the *Build Tools for Visual Studio*
   (or full VS Community) with the **"Desktop development with C++"** workload.
   This provides the MSVC linker (`link.exe`) and the Windows SDK that the
   `*-windows-msvc` target links against.
2. **rustup** — install from <https://rustup.rs>. Accept the default host
   triple `x86_64-pc-windows-msvc`:
   ```powershell
   rustup default stable-x86_64-pc-windows-msvc
   rustup component add clippy rustfmt
   ```
3. Verify:
   ```powershell
   rustc --version
   cargo --version
   ```

## 2. Build the core + CLI

```powershell
git clone https://github.com/quazardous/runic.git
cd runic
cargo build            # builds the `runic` library + the `runic` CLI binary
cargo test             # the full suite is platform-agnostic and runs on Windows
```

The CLI binary lands at `target\debug\runic.exe` (or `target\release\runic.exe`
with `--release`). It behaves exactly like the Linux build.

## 3. The `runic-tray` crate (v1, to build here)

The tray app is a **second front-end over the `runic` library** — same core,
different shell. The intended shape:

- A new crate `runic-tray` (sibling crate; promote the repo to a Cargo
  workspace with members `runic` + `runic-tray` when you start it) that depends
  on the core:
  ```toml
  # runic-tray/Cargo.toml
  [dependencies]
  runic = { path = ".." }            # or the workspace member path
  tao = "0.30"                       # cross-platform event loop (winit fork)
  tray-icon = "0.19"                 # native Shell_NotifyIcon tray
  tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync"] }
  ```
- Mark it a GUI app (no console window) at the crate root:
  ```rust
  #![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
  ```
- Spawn the core server in a tokio task and drive it from the tray menu. The
  core already broadcasts its config over a `watch` channel and exposes the
  admin/store API the menu actions map onto:
  - **Start / Stop / Restart** — own the tokio runtime + the server task.
  - **Status** — running/stopped + last error.
  - **Show current IP** — internal request through the local SOCKS5 to
    `api.ipify.org`, shown in a toast (`tray-icon` / `notify-rust`).
  - **Open config / Show logs / Quit**.
- Config location on Windows: `%APPDATA%\runic\runic.yaml` (+ `creds.env`).

The menu actions are thin wrappers over the existing library surface — start
from `runic::server::run`, `runic::store::ConfigStore`, and the admin endpoints
in `runic::admin`.

## 4. Auto-start at login

Either write the run key on first launch:

```
HKCU\Software\Microsoft\Windows\CurrentVersion\Run
  runic = "C:\Path\to\runic-tray.exe"
```

or drop a shortcut in the user's `Startup` folder
(`%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`). Expose it as a
menu toggle so the user opts in.

## 5. Packaging

- **MSI** via [`cargo-wix`](https://github.com/volks73/cargo-wix):
  ```powershell
  cargo install cargo-wix
  cargo wix --package runic-tray
  ```
- or a **portable ZIP** of `runic-tray.exe` + a default `runic.yaml`.

## 6. Cross-compiling from Linux (optional)

You can produce a Windows `.exe` from a Linux box with
[`cargo-xwin`](https://github.com/rust-cross/cargo-xwin) (no Windows machine
needed for the build — but you still need Windows to actually run/smoke-test the
tray UI):

```bash
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin
cargo xwin build --release --target x86_64-pc-windows-msvc
```

The CLI cross-compiles cleanly this way. The tray crate's GUI deps (`tao`,
`tray-icon`) also support this target, but the resulting binary must be
exercised on real Windows — tray icon, menu, toasts and auto-start can't be
verified from Linux.

## Notes

- Keep the tray crate's logic thin: anything non-UI belongs in the `runic`
  library so it stays testable on Linux CI.
- macOS is out of scope; Linux uses the systemd path (see
  [`../install/linux-systemd.md`](../install/linux-systemd.md)).
