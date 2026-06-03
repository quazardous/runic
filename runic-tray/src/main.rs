// runic-tray — Windows system-tray front-end over the `runic` core library.
//
// Built and smoke-tested on Windows 11 (deps tao 0.30 / tray-icon 0.19 /
// windows 0.58; builds on both the GNU and MSVC toolchains). See
// ../docs/dev/windows-setup.md.
//
// Architecture: the tray is a thin shell. All proxy logic lives in the `runic`
// library (start/stop the SOCKS5 server, config store, admin API). The tao
// event loop runs on the main thread; menu clicks are forwarded to it via an
// EventLoopProxy and drive a `Supervisor` that owns a tokio runtime. The
// app-lifetime infrastructure (config store, watcher, admin API) is brought up
// once; Start/Stop only cycles the SOCKS5 data plane.

// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

use runic::{admin, config, server, store, watcher};

/// Owns the tokio runtime and the running proxy tasks. Start spins the core up;
/// Stop aborts the SOCKS5 server task.
struct Supervisor {
    rt: tokio::runtime::Runtime,
    config_path: PathBuf,
    /// App-lifetime config receiver, set by `boot` once the ConfigStore + watcher
    /// + admin listeners are up. Cloned into each server task.
    cfg_rx: Option<tokio::sync::watch::Receiver<Arc<config::Config>>>,
    server_task: Option<JoinHandle<()>>,
}

impl Supervisor {
    fn new(config_path: PathBuf) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            rt,
            config_path,
            cfg_rx: None,
            server_task: None,
        })
    }

    fn is_running(&self) -> bool {
        self.server_task
            .as_ref()
            .map(|t| !t.is_finished())
            .unwrap_or(false)
    }

    /// Bring up the app-lifetime infrastructure exactly once: cold load →
    /// ConfigStore → watcher + admin API. These listeners (notably the admin API)
    /// live for the whole process — Start/Stop only cycles the SOCKS5 data plane,
    /// so they must NOT be re-bound on every start (that was the Stop→Start
    /// "address already in use" bug). Idempotent: a second call is a no-op.
    fn boot(&mut self) -> Result<()> {
        if self.cfg_rx.is_some() {
            return Ok(());
        }
        let path = self.config_path.clone();
        let cfg_rx = self.rt.block_on(async move {
            // NOTE: the tray runs the core in plain (non-silo) mode. Silo mode is
            // opt-in and adds the encrypted per-variation store + sweeper; wire it
            // here later mirroring the CLI `main` (pass the cache to admin::spawn
            // and server::run) if the tray ever needs it. `_silo_cfg` is ignored.
            let (cfg, admin_cfg, _silo_cfg) = config::Config::load_with_admin(&path)?;
            let snapshot_path = store::default_snapshot_path();
            let (cfg_store, cfg_rx) = store::ConfigStore::new(cfg, snapshot_path);
            let cfg_store = Arc::new(Mutex::new(cfg_store));

            watcher::spawn(path.clone(), cfg_store.clone())?;
            admin::spawn(admin_cfg.addr, cfg_store.clone(), None).await?;
            Ok::<_, anyhow::Error>(cfg_rx)
        })?;
        self.cfg_rx = Some(cfg_rx);
        Ok(())
    }

    /// Start the SOCKS5 data plane. Boots the app-lifetime infra on first call,
    /// then spawns the server task (kept so Stop can abort it). No-op if already
    /// running. Config changes flow in live via the watcher, so a restart needs
    /// no reload.
    fn start(&mut self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        self.boot()?;
        let cfg_rx = self
            .cfg_rx
            .clone()
            .expect("cfg_rx is set by boot() above");
        let task = self.rt.handle().spawn(async move {
            if let Err(e) = server::run(cfg_rx, None).await {
                tracing::error!(error = %e, "server exited with error");
            }
        });
        self.server_task = Some(task);
        Ok(())
    }

    /// Abort the SOCKS5 server task (frees its listener). The admin API + watcher
    /// stay up for the process lifetime.
    fn stop(&mut self) {
        if let Some(task) = self.server_task.take() {
            task.abort();
            // Wait for the task to unwind so its SOCKS5 listener is dropped (port
            // freed) before any subsequent start() rebinds it. Aborted join =
            // Err(Cancelled), ignored.
            let _ = self.rt.block_on(task);
        }
    }

    fn restart(&mut self) -> Result<()> {
        self.stop();
        self.start()
    }
}

/// The Raidho rune (ᚱ — "journey / the right route", the traffic passing through)
/// as line segments in a normalized [0,1] box: (x0, y0)->(x1, y1). One geometry,
/// rasterized at any size and colour, so every state variant below derives from
/// this single source of truth.
const RAIDHO_STROKES: &[(f32, f32, f32, f32)] = &[
    (0.20, 0.06, 0.20, 0.94), // stave — left vertical, full height
    (0.20, 0.06, 0.66, 0.28), // top of stave -> upper-right peak
    (0.66, 0.28, 0.20, 0.50), // peak -> middle of stave (closes the bowl)
    (0.20, 0.50, 0.70, 0.94), // middle -> bottom-right leg
];

/// Tray icon state. Same Raidho glyph, different tint — the colour carries the
/// runtime state at a glance on the taskbar (readable on light + dark themes).
#[derive(Clone, Copy)]
enum IconState {
    /// Proxy not running.
    Stopped,
    /// Up and proxied (traffic anonymised via an upstream) — the healthy state.
    Running,
    /// Up but a `kind:direct` upstream is active: local IP EXPOSED, not proxied.
    /// Amber on purpose — the user must never mistake this for anonymised.
    #[allow(dead_code)] // wired once the core surfaces the active-upstream kind
    Direct,
    /// Up with an empty pool / no route yet (boot-empty; sessions declined until
    /// configured over the admin API).
    #[allow(dead_code)] // wired once the core surfaces "no route"
    NoRoute,
    /// Failed to start / degraded.
    Error,
}

impl IconState {
    fn rgb(self) -> [u8; 3] {
        match self {
            IconState::Stopped => [0x8a, 0x8a, 0x8a], // neutral grey
            IconState::Running => [0x14, 0x9e, 0x9e], // teal
            IconState::Direct => [0xe0, 0x8a, 0x16],  // amber — warning
            IconState::NoRoute => [0x3b, 0x82, 0xf6], // blue — waiting
            IconState::Error => [0xd6, 0x3b, 0x3b],   // red
        }
    }
}

/// Render the Raidho rune as a tray icon: just the glyph (no background, no
/// decoration), transparent elsewhere, tinted by `state`. Drawn at `size` px
/// with a stroke ~`size/8` thick; crisp without antialiasing at 16–32 px.
fn raidho_icon(state: IconState, size: u32) -> Result<Icon> {
    let [r, g, b] = state.rgb();
    let mut rgba = vec![0u8; (size * size * 4) as usize]; // fully transparent
    let thickness = (size as f32 / 8.0).max(1.5);
    for &(x0, y0, x1, y1) in RAIDHO_STROKES {
        draw_segment(
            &mut rgba,
            size,
            (x0 * size as f32, y0 * size as f32),
            (x1 * size as f32, y1 * size as f32),
            thickness,
            [r, g, b, 0xff],
        );
    }
    Ok(Icon::from_rgba(rgba, size, size)?)
}

/// Stamp a thick line into `buf` via a per-pixel distance-to-segment test (each
/// pixel within `thickness/2` of the segment is filled). Cheap and adequate for
/// icon-sized glyphs.
fn draw_segment(
    buf: &mut [u8],
    size: u32,
    (x0, y0): (f32, f32),
    (x1, y1): (f32, f32),
    thickness: f32,
    rgba: [u8; 4],
) {
    let half = thickness / 2.0;
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len2 = (dx * dx + dy * dy).max(1e-6);
    for py in 0..size {
        for px in 0..size {
            let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
            let t = (((fx - x0) * dx + (fy - y0) * dy) / len2).clamp(0.0, 1.0);
            let (cx, cy) = (x0 + t * dx, y0 + t * dy);
            let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            if dist <= half {
                let i = ((py * size + px) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&rgba);
            }
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RUNIC_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("runic=info")),
        )
        .init();

    // Shared with the CLI: `%APPDATA%\runic\runic.yaml` on Windows (single
    // source of truth in `runic::paths`, no divergent copy here).
    let mut supervisor = Supervisor::new(runic::paths::default_config_path())?;
    // Auto-start the proxy on launch (the tray reflects state; user can Stop).
    if let Err(e) = supervisor.start() {
        tracing::error!(error = %e, "initial start failed");
    }

    // Build the tray menu.
    let menu = Menu::new();
    let start_i = MenuItem::new("Start", true, None);
    let stop_i = MenuItem::new("Stop", true, None);
    let restart_i = MenuItem::new("Restart", true, None);
    let status_i = MenuItem::new("Status", true, None);
    let ip_i = MenuItem::new("Show current IP", true, None);
    let config_i = MenuItem::new("Open config file", true, None);
    let logs_i = MenuItem::new("Show logs", true, None);
    let quit_i = MenuItem::new("Quit", true, None);
    menu.append_items(&[
        &start_i,
        &stop_i,
        &restart_i,
        &PredefinedMenuItem::separator(),
        &status_i,
        &ip_i,
        &config_i,
        &logs_i,
        &PredefinedMenuItem::separator(),
        &quit_i,
    ])?;

    // Event loop with a user-event channel; forward menu clicks into it so the
    // loop wakes on menu activity (Wait control flow otherwise sleeps).
    let event_loop = EventLoopBuilder::<MenuEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = proxy.send_event(e);
    }));

    // Icon size: 32 px renders crisply and Windows downscales for the taskbar.
    const ICON_PX: u32 = 32;
    let initial_state = if supervisor.is_running() {
        IconState::Running
    } else {
        IconState::Stopped
    };
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("runic")
        .with_icon(raidho_icon(initial_state, ICON_PX)?)
        .build()?;

    // Reflect a state change on the tray icon (best-effort; a render/set failure
    // must not take the loop down).
    let set_state = move |tray: &tray_icon::TrayIcon, state: IconState| {
        if let Ok(icon) = raidho_icon(state, ICON_PX) {
            let _ = tray.set_icon(Some(icon));
        }
    };

    let config_path = runic::paths::default_config_path();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let tao::event::Event::UserEvent(menu_event) = event {
            match menu_event.id {
                id if id == start_i.id() => match supervisor.start() {
                    Ok(()) => set_state(&tray, IconState::Running),
                    Err(e) => {
                        tracing::error!(error = %e, "start failed");
                        set_state(&tray, IconState::Error);
                    }
                },
                id if id == stop_i.id() => {
                    supervisor.stop();
                    set_state(&tray, IconState::Stopped);
                }
                id if id == restart_i.id() => match supervisor.restart() {
                    Ok(()) => set_state(&tray, IconState::Running),
                    Err(e) => {
                        tracing::error!(error = %e, "restart failed");
                        set_state(&tray, IconState::Error);
                    }
                },
                id if id == status_i.id() => {
                    let state = if supervisor.is_running() { "running" } else { "stopped" };
                    tracing::info!(%state, "status");
                    // TODO: surface via a native Windows toast instead of a log line.
                }
                id if id == ip_i.id() => {
                    // TODO: issue an internal request through the local SOCKS5 to
                    // api.ipify.org on the tokio runtime and show the IP in a toast.
                }
                id if id == config_i.id() => {
                    // TODO: open the YAML in the default editor
                    // (std::process::Command "cmd /C start" on the config path).
                    let _ = &config_path;
                }
                id if id == logs_i.id() => {
                    // TODO: open the log file / a simple log window.
                }
                id if id == quit_i.id() => {
                    supervisor.stop();
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        }
    });
}
