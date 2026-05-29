// runic-tray — Windows system-tray front-end over the `runic` core library.
//
// ⚠️ AUTHORED FROM SPEC, NOT COMPILED HERE. The dev sandbox has no Windows
// toolchain and can't fetch the GUI deps, so this skeleton has not been built.
// Build + smoke it on Windows (see ../docs/dev/windows-setup.md) and fix any
// API drift in `tao` / `tray-icon` versions on first compile.
//
// Architecture: the tray is a thin shell. All proxy logic lives in the `runic`
// library (start/stop the SOCKS5 server, config store, admin API). The tao
// event loop runs on the main thread; menu clicks are forwarded to it via an
// EventLoopProxy and drive a `Supervisor` that owns a tokio runtime.

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
            server_task: None,
        })
    }

    fn is_running(&self) -> bool {
        self.server_task
            .as_ref()
            .map(|t| !t.is_finished())
            .unwrap_or(false)
    }

    /// Boot the core: cold load → ConfigStore → watcher + admin + server. Mirrors
    /// the CLI's `main`. The server task is kept so Stop can abort it.
    fn start(&mut self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let path = self.config_path.clone();
        let handle = self.rt.handle().clone();
        let server_task = self.rt.block_on(async move {
            let (cfg, admin_cfg) = config::Config::load_with_admin(&path)?;
            let snapshot_path = store::default_snapshot_path();
            let (cfg_store, cfg_rx) = store::ConfigStore::new(cfg, snapshot_path);
            let cfg_store = Arc::new(Mutex::new(cfg_store));

            // NOTE: watcher + admin spawn detached tasks; the current lib API
            // doesn't hand back shutdown handles for them, so a Stop only halts
            // the SOCKS5 server. TODO(lib): expose shutdown handles for clean
            // restart of the watcher/admin listeners.
            watcher::spawn(path.clone(), cfg_store.clone())?;
            admin::spawn(admin_cfg.addr, cfg_store.clone()).await?;

            let task = handle.spawn(async move {
                if let Err(e) = server::run(cfg_rx).await {
                    tracing::error!(error = %e, "server exited with error");
                }
            });
            Ok::<_, anyhow::Error>(task)
        })?;
        self.server_task = Some(server_task);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }

    fn restart(&mut self) -> Result<()> {
        self.stop();
        self.start()
    }
}

/// Default config path on Windows: %APPDATA%\runic\runic.yaml.
fn default_config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("runic")
        .join("runic.yaml")
}

/// A 16x16 solid-teal placeholder icon. TODO: ship a proper .ico asset.
fn placeholder_icon() -> Result<Icon> {
    let (w, h) = (16u32, 16u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[0x14, 0x9e, 0x9e, 0xff]); // teal
    }
    Ok(Icon::from_rgba(rgba, w, h)?)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RUNIC_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("runic=info")),
        )
        .init();

    let mut supervisor = Supervisor::new(default_config_path())?;
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

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("runic")
        .with_icon(placeholder_icon()?)
        .build()?;

    let config_path = default_config_path();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let tao::event::Event::UserEvent(menu_event) = event {
            match menu_event.id {
                id if id == start_i.id() => {
                    if let Err(e) = supervisor.start() {
                        tracing::error!(error = %e, "start failed");
                    }
                }
                id if id == stop_i.id() => supervisor.stop(),
                id if id == restart_i.id() => {
                    if let Err(e) = supervisor.restart() {
                        tracing::error!(error = %e, "restart failed");
                    }
                }
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
