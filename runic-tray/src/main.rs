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

mod autostart;
mod prefs;
mod toast;
mod update;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tao::event::StartCause;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

use runic::{admin, config, server, stats, store, watcher};

/// How often the tray polls the admin status API to refresh the icon colour.
const POLL_EVERY: Duration = Duration::from_secs(3);

/// Active-route classification polled from `GET /v1/status`, stored in a shared
/// atomic so the event loop can recolour the icon without blocking on I/O.
const ROUTE_UNKNOWN: u8 = 0; // status not reachable yet
const ROUTE_PROXIED: u8 = 1; // active route exits through a proxy upstream
const ROUTE_DIRECT: u8 = 2; // a direct (non-proxied) route is active — local IP exposed
const ROUTE_NOROUTE: u8 = 3; // no active route (empty pool / boot-empty)

/// Events delivered into the tao loop. Menu clicks arrive from the global menu
/// handler; `Toast` lets a background tokio task (e.g. Show IP) marshal a result
/// string back so the toast is shown on the UI thread, where COM is initialised.
enum UserEvent {
    Menu(MenuEvent),
    Toast {
        title: String,
        body: String,
    },
    /// Result of an update check, marshaled back from a background task.
    /// `manual` distinguishes a user-triggered check (toast even when up to
    /// date) from the silent startup check (toast only when an update exists).
    UpdateChecked {
        update: Option<update::Update>,
        manual: bool,
    },
    /// Clean shutdown requested from a background task (e.g. after launching the
    /// installer, the tray must exit to unlock its exe).
    Quit,
}

/// A minimal config written on first "Open config file" when none exists yet.
/// Fully commented out — every key shows its built-in default, and a
/// comment-only file is a valid config (boot-empty model: upstreams are
/// pushed later over the admin API).
const CONFIG_EXAMPLE: &str = "\
# runic configuration — SOCKS5 listener + upstreams.
# Each key below shows its BUILT-IN DEFAULT, applied while the key stays
# commented; uncomment only what you change. The tray runs boot-empty: add
# upstreams here or push them over the admin API.
# Docs: https://github.com/quazardous/runic
# Port 0 (the default) = auto mode: the OS picks the SOCKS5 port; read the
# real one on the status page. Uncomment addr with an explicit port to pin
# it, or keep port 0 and set port_range to scan a window instead.
# listen:
#   addr: \"127.0.0.1:0\"
#   port_range: \"20000-20100\"
#   auth: none
# admin:
#   addr: \"127.0.0.1:48484\"
# upstreams: {}
";

/// Owns the tokio runtime and the running proxy tasks. Start spins the core up;
/// Stop aborts the SOCKS5 server task.
struct Supervisor {
    rt: tokio::runtime::Runtime,
    config_path: PathBuf,
    /// App-lifetime config receiver, set by `boot` once the ConfigStore + watcher
    /// + admin listeners are up. Cloned into each server task.
    cfg_rx: Option<tokio::sync::watch::Receiver<Arc<config::Config>>>,
    /// App-lifetime live session counters, shared by the admin status endpoint
    /// and each SOCKS5 server task.
    stats: Arc<stats::Stats>,
    /// Loopback address of the admin API, captured at boot — the tray polls
    /// `GET /v1/status` here to colour the icon by active route.
    admin_addr: Option<SocketAddr>,
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
            stats: stats::Stats::new(),
            admin_addr: None,
            server_task: None,
        })
    }

    fn is_running(&self) -> bool {
        self.server_task
            .as_ref()
            .map(|t| !t.is_finished())
            .unwrap_or(false)
    }

    /// Current SOCKS5 listen address from the live config (None before boot).
    /// Read fresh each call so a config change flowing through the watcher is
    /// reflected — "Show current IP" dials this to reach the proxy.
    fn socks_addr(&self) -> Option<SocketAddr> {
        self.cfg_rx.as_ref().map(|rx| rx.borrow().listen.addr)
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
        let stats = self.stats.clone();
        let (cfg_rx, admin_addr) = self.rt.block_on(async move {
            // NOTE: the tray runs the core in plain (non-silo) mode. Silo mode is
            // opt-in and adds the encrypted per-variation store + sweeper; wire it
            // here later mirroring the CLI `main` (pass the cache to admin::spawn
            // and server::run) if the tray ever needs it. `_silo_cfg` is ignored.
            let (cfg, admin_cfg, _silo_cfg) = config::Config::load_with_admin(&path)?;
            let snapshot_path = store::default_snapshot_path();
            let (cfg_store, cfg_rx) = store::ConfigStore::new(cfg, snapshot_path);
            let cfg_store = Arc::new(Mutex::new(cfg_store));

            watcher::spawn(path.clone(), cfg_store.clone())?;
            admin::spawn(admin_cfg.addr, cfg_store.clone(), None, stats).await?;
            Ok::<_, anyhow::Error>((cfg_rx, admin_cfg.addr))
        })?;
        self.cfg_rx = Some(cfg_rx);
        self.admin_addr = Some(admin_addr);
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
        let cfg_rx = self.cfg_rx.clone().expect("cfg_rx is set by boot() above");
        let stats = self.stats.clone();
        let task = self.rt.handle().spawn(async move {
            if let Err(e) = server::run(cfg_rx, None, stats).await {
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum IconState {
    /// Proxy not running.
    Stopped,
    /// Up and proxied (traffic anonymised via an upstream) — the healthy state.
    Running,
    /// Up but a `kind:direct` upstream is active: local IP EXPOSED, not proxied.
    /// Amber on purpose — the user must never mistake this for anonymised.
    Direct,
    /// Up with an empty pool / no route yet (boot-empty; sessions declined until
    /// configured over the admin API).
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

    /// Hover text that spells out what the icon colour means, so the state is
    /// discoverable without knowing the colour legend.
    fn tooltip(self) -> &'static str {
        match self {
            IconState::Stopped => "runic — stopped",
            IconState::Running => "runic — proxied",
            IconState::Direct => "runic — DIRECT: real IP exposed",
            IconState::NoRoute => "runic — no route (empty pool)",
            IconState::Error => "runic — start error",
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

/// Poll `GET /v1/status` on the loopback admin API and classify the active route
/// for the tray icon. Returns a `ROUTE_*` code; `ROUTE_UNKNOWN` on any failure.
///
/// Conservative on exposure: amber (`ROUTE_DIRECT`) as soon as the status
/// reports any active direct session OR a direct active route — the user must
/// never see "proxied" while their IP is exposed.
async fn fetch_route(addr: SocketAddr) -> u8 {
    async fn inner(addr: SocketAddr) -> Option<u8> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
        stream
            .write_all(b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut buf = Vec::with_capacity(2048);
        stream.read_to_end(&mut buf).await.ok()?;
        let text = String::from_utf8_lossy(&buf);
        // The body is the outermost JSON object; this skips HTTP headers and any
        // Content-Length / single-chunk framing without a full HTTP parser.
        let json = &text[text.find('{')?..=text.rfind('}')?];
        let v: serde_json::Value = serde_json::from_str(json).ok()?;

        let any_direct = v
            .get("any_active_direct")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let route_is_direct = v
            .get("active_route")
            .and_then(|r| r.get("kind"))
            .and_then(|k| k.as_str())
            == Some("direct");
        let no_route = v.get("active_route").map(|r| r.is_null()).unwrap_or(true);

        Some(if any_direct || route_is_direct {
            ROUTE_DIRECT
        } else if no_route {
            ROUTE_NOROUTE
        } else {
            ROUTE_PROXIED
        })
    }
    inner(addr).await.unwrap_or(ROUTE_UNKNOWN)
}

/// Fetch the public IP as seen THROUGH the local SOCKS5 proxy (so it reflects
/// the active upstream, not the host). Speaks just enough SOCKS5 by hand and
/// uses the domain address type so the proxy resolves `api.ipify.org` — no
/// local DNS, and it works even when the host can't reach the internet directly.
async fn fetch_public_ip(socks: SocketAddr) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // A 0.0.0.0 bind isn't connectable — dial loopback on the same port.
    let target = if socks.ip().is_unspecified() {
        SocketAddr::new(std::net::IpAddr::from([127, 0, 0, 1]), socks.port())
    } else {
        socks
    };
    let mut s = tokio::net::TcpStream::connect(target).await?;

    // Greeting: VER=5, one method, NO-AUTH.
    s.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut method = [0u8; 2];
    s.read_exact(&mut method).await?;
    anyhow::ensure!(method == [0x05, 0x00], "SOCKS5 no-auth negotiation failed");

    // CONNECT api.ipify.org:80 (ATYP=domain → the proxy resolves the host).
    let host = b"api.ipify.org";
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    req.extend_from_slice(host);
    req.extend_from_slice(&80u16.to_be_bytes());
    s.write_all(&req).await?;

    // Reply: VER REP RSV ATYP BND.ADDR BND.PORT — REP 0 = success.
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        // Map the RFC 1928 reply code to something readable (code 1 = the proxy
        // had no way to route — usually an empty pool, handled before we get here).
        let reason = match head[1] {
            1 => "general proxy failure (no route?)",
            2 => "connection not allowed by ruleset",
            3 => "network unreachable",
            4 => "host unreachable",
            5 => "connection refused",
            6 => "TTL expired",
            7 => "command not supported",
            8 => "address type not supported",
            c => return Err(anyhow::anyhow!("SOCKS5 rejected (code {c})")),
        };
        anyhow::bail!("{reason}");
    }
    let bnd = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            len[0] as usize
        }
        other => anyhow::bail!("unexpected SOCKS5 ATYP {other}"),
    };
    let mut bound = vec![0u8; bnd + 2]; // addr + port, discarded
    s.read_exact(&mut bound).await?;

    // ipify returns the bare IP as the body.
    s.write_all(
        b"GET / HTTP/1.1\r\nHost: api.ipify.org\r\nConnection: close\r\nUser-Agent: runic-tray\r\n\r\n",
    )
    .await?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let ip = text
        .split("\r\n\r\n")
        .nth(1)
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| anyhow::anyhow!("empty response from ipify"))?;
    Ok(ip.to_string())
}

/// Open a path with its Windows default handler. Last resort only: `cmd /C
/// start` briefly flashes a console window, so the text-file openers below
/// prefer Notepad and fall back here only if it can't be launched.
fn open_path(path: &Path) -> Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", &path.to_string_lossy()])
        .spawn()?;
    Ok(())
}

/// Open a (text) file in Notepad. Notepad is a GUI app, so unlike `cmd /C start`
/// it opens with no flashing console window; and the default `.yaml`/`.log`
/// handler is often a heavy editor (VS Code). Falls back to the default handler
/// only if Notepad can't be launched (it ships with every Windows).
fn open_in_notepad(path: &Path) -> Result<()> {
    if std::process::Command::new("notepad.exe")
        .arg(path)
        .spawn()
        .is_ok()
    {
        Ok(())
    } else {
        open_path(path)
    }
}

/// Open the config file, seeding a commented example if it doesn't exist yet.
fn open_config(path: &Path) -> Result<()> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, CONFIG_EXAMPLE)?;
    }
    open_in_notepad(path)
}

/// Initialise tracing to a file so logs survive `windows_subsystem="windows"`
/// (no console in release). Returns the log path for "Show logs", plus the
/// appender guard, which the caller must keep alive for the process lifetime so
/// the non-blocking writer keeps flushing.
fn init_logging() -> (
    Option<PathBuf>,
    Option<tracing_appender::non_blocking::WorkerGuard>,
) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("RUNIC_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("runic=info"));
    let dir = runic::paths::default_config_path()
        .parent()
        .map(Path::to_path_buf);
    match dir {
        Some(dir) if std::fs::create_dir_all(&dir).is_ok() => {
            let (writer, guard) =
                tracing_appender::non_blocking(tracing_appender::rolling::never(&dir, LOG_FILE));
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(writer)
                .init();
            (Some(dir.join(LOG_FILE)), Some(guard))
        }
        _ => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            (None, None)
        }
    }
}

const LOG_FILE: &str = "runic-tray.log";

/// Run an update check on a blocking pool thread (ureq is blocking) and marshal
/// the result back into the event loop. `manual` true = a user-clicked check
/// (report even when up to date / on error); false = the silent startup check.
fn spawn_update_check(
    handle: &tokio::runtime::Handle,
    proxy: tao::event_loop::EventLoopProxy<UserEvent>,
    manual: bool,
) {
    handle.spawn_blocking(move || match update::check() {
        Ok(update) => {
            let _ = proxy.send_event(UserEvent::UpdateChecked { update, manual });
        }
        Err(e) => {
            tracing::warn!(error = %e, "update check failed");
            if manual {
                let _ = proxy.send_event(UserEvent::Toast {
                    title: "runic — updates".to_string(),
                    body: format!("check failed: {e}"),
                });
            }
        }
    });
}

fn main() -> Result<()> {
    let (log_path, _log_guard) = init_logging();

    // Register runic's own AppUserModelID so toasts read "runic" (not "Windows
    // PowerShell") — must run before the first toast.
    toast::init();

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
    let check_i = MenuItem::new("Check for updates", true, None);
    // Disabled until a check finds a newer release; then relabeled + enabled.
    let update_i = MenuItem::new("Install update", false, None);
    // Opt-in: check for updates at startup (persisted, mirrors the real pref).
    let autoupdate_i = CheckMenuItem::new("Auto-update", true, prefs::auto_update_enabled(), None);
    // Opt-in launch-at-login; the checkmark mirrors the actual Run-key state.
    let autostart_i = CheckMenuItem::new("Start with Windows", true, autostart::is_enabled(), None);
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
        &check_i,
        &update_i,
        &autoupdate_i,
        &autostart_i,
        &PredefinedMenuItem::separator(),
        &quit_i,
    ])?;

    // Event loop with a user-event channel; forward menu clicks into it so the
    // loop wakes on menu activity (Wait control flow otherwise sleeps).
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));
    // A second proxy the loop hands to background tasks (Show IP) to marshal a
    // toast string back onto the UI thread.
    let toast_proxy = event_loop.create_proxy();

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

    // Reflect a state change on the tray icon + tooltip (best-effort; a render/
    // set failure must not take the loop down). The tooltip spells out the
    // colour so hovering tells you the state in words.
    let set_state = move |tray: &tray_icon::TrayIcon, state: IconState| {
        if let Ok(icon) = raidho_icon(state, ICON_PX) {
            let _ = tray.set_icon(Some(icon));
        }
        let _ = tray.set_tooltip(Some(state.tooltip()));
    };

    // Background: poll the admin status API and stash the active-route class in a
    // shared atomic; the event loop reads it on a timer to recolour the icon
    // (Direct = amber, NoRoute = blue) without ever blocking the UI thread on I/O.
    let route = Arc::new(AtomicU8::new(ROUTE_UNKNOWN));
    if let Some(addr) = supervisor.admin_addr {
        let route = route.clone();
        supervisor.rt.handle().spawn(async move {
            let mut tick = tokio::time::interval(POLL_EVERY);
            loop {
                tick.tick().await;
                route.store(fetch_route(addr).await, Ordering::Relaxed);
            }
        });
    }

    let config_path = runic::paths::default_config_path();

    // The "popup that identifies it" the icon otherwise lacks — it lands
    // silently (Windows 11 even hides it in the overflow).
    toast::toast(
        "runic ᚱ",
        if matches!(initial_state, IconState::Running) {
            "started"
        } else {
            "started (proxy stopped)"
        },
    );

    // Opt-in: only check at startup if the user enabled auto-update. Silent
    // unless a newer release exists.
    if prefs::auto_update_enabled() {
        spawn_update_check(supervisor.rt.handle(), toast_proxy.clone(), false);
    }

    // Last start/restart error, surfaced by the Status toast.
    let mut last_error: Option<String> = None;
    // Newest release found by a check, applied when "Install update" is clicked.
    let mut pending_update: Option<update::Update> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL_EVERY);

        match event {
            // Periodic refresh: recolour the icon from the polled route. Stopped
            // overrides everything; otherwise the active-route class picks the
            // colour (Direct = amber / NoRoute = blue / proxied = teal).
            tao::event::Event::NewEvents(
                StartCause::ResumeTimeReached { .. } | StartCause::Init,
            ) => {
                let state = if !supervisor.is_running() {
                    IconState::Stopped
                } else {
                    match route.load(Ordering::Relaxed) {
                        ROUTE_DIRECT => IconState::Direct,
                        ROUTE_NOROUTE => IconState::NoRoute,
                        _ => IconState::Running,
                    }
                };
                set_state(&tray, state);
            }
            // A background task (Show IP) finished and handed back a toast.
            tao::event::Event::UserEvent(UserEvent::Toast { title, body }) => {
                toast::toast(&title, &body);
            }
            // An update check completed (startup or manual).
            tao::event::Event::UserEvent(UserEvent::UpdateChecked { update, manual }) => {
                match update {
                    Some(u) => {
                        update_i.set_text(format!("Install update v{}", u.version));
                        update_i.set_enabled(true);
                        toast::toast(
                            "runic — updates",
                            &format!("update v{} available — use \"Install update\"", u.version),
                        );
                        pending_update = Some(u);
                    }
                    None if manual => toast::toast(
                        "runic — updates",
                        &format!("runic is up to date (v{})", update::current()),
                    ),
                    None => {}
                }
            }
            // A background task asked for a clean exit (e.g. after launching the
            // installer, which must replace this exe).
            tao::event::Event::UserEvent(UserEvent::Quit) => {
                supervisor.stop();
                *control_flow = ControlFlow::Exit;
            }
            tao::event::Event::UserEvent(UserEvent::Menu(menu_event)) => {
                match menu_event.id {
                    id if id == start_i.id() => match supervisor.start() {
                        Ok(()) => set_state(&tray, IconState::Running),
                        Err(e) => {
                            tracing::error!(error = %e, "start failed");
                            last_error = Some(e.to_string());
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
                            last_error = Some(e.to_string());
                            set_state(&tray, IconState::Error);
                        }
                    },
                    id if id == status_i.id() => {
                        let body = if supervisor.is_running() {
                            match route.load(Ordering::Relaxed) {
                                ROUTE_DIRECT => "running — DIRECT route (IP exposed)".to_string(),
                                ROUTE_NOROUTE => "running — no route (empty pool)".to_string(),
                                _ => "running — proxied".to_string(),
                            }
                        } else if let Some(e) = &last_error {
                            format!("stopped — last error: {e}")
                        } else {
                            "stopped".to_string()
                        };
                        toast::toast("runic — status", &body);
                    }
                    id if id == ip_i.id() => match supervisor.socks_addr() {
                        _ if !supervisor.is_running() => {
                            toast::toast("runic — IP", "proxy stopped — start it first")
                        }
                        // No active route (empty pool / boot-empty): the proxy
                        // would reject the CONNECT with a bare "code 1". Say what
                        // actually needs doing instead.
                        _ if route.load(Ordering::Relaxed) == ROUTE_NOROUTE => toast::toast(
                            "runic — IP",
                            "no active route (empty pool) — add an upstream to route traffic",
                        ),
                        Some(addr) => {
                            // Network I/O on the runtime; the result is marshaled
                            // back as a Toast event so it shows on the UI thread.
                            let proxy = toast_proxy.clone();
                            supervisor.rt.handle().spawn(async move {
                                let (title, body) = match fetch_public_ip(addr).await {
                                    Ok(ip) => ("runic — public IP", ip),
                                    Err(e) => ("runic — IP", format!("failed: {e}")),
                                };
                                let _ = proxy.send_event(UserEvent::Toast {
                                    title: title.to_string(),
                                    body,
                                });
                            });
                        }
                        None => toast::toast("runic — IP", "proxy not initialised yet"),
                    },
                    id if id == config_i.id() => {
                        if let Err(e) = open_config(&config_path) {
                            tracing::error!(error = %e, "open config failed");
                            toast::toast("runic", &format!("could not open config: {e}"));
                        }
                    }
                    id if id == logs_i.id() => match &log_path {
                        Some(p) => {
                            if let Err(e) = open_in_notepad(p) {
                                tracing::error!(error = %e, "open logs failed");
                            }
                        }
                        None => toast::toast("runic", "log file unavailable"),
                    },
                    id if id == check_i.id() => {
                        toast::toast("runic — updates", "checking…");
                        spawn_update_check(supervisor.rt.handle(), toast_proxy.clone(), true);
                    }
                    id if id == update_i.id() => {
                        if let Some(u) = pending_update.clone() {
                            toast::toast(
                                "runic — updates",
                                &format!("downloading update v{}…", u.version),
                            );
                            let proxy = toast_proxy.clone();
                            supervisor.rt.handle().spawn_blocking(move || {
                                match update::apply(&u) {
                                    Ok(()) => {
                                        let _ = proxy.send_event(UserEvent::Toast {
                                            title: "runic — updates".to_string(),
                                            body: "installer launched — the tray will close"
                                                .to_string(),
                                        });
                                        let _ = proxy.send_event(UserEvent::Quit);
                                    }
                                    Err(e) => {
                                        tracing::error!(error = %e, "update apply failed");
                                        let _ = proxy.send_event(UserEvent::Toast {
                                            title: "runic — updates".to_string(),
                                            body: format!("update failed: {e}"),
                                        });
                                    }
                                }
                            });
                        }
                    }
                    id if id == autostart_i.id() => {
                        // Target the opposite of what's actually persisted, write it,
                        // then force the checkmark to mirror reality so a failed write
                        // never leaves the UI lying.
                        let target = !autostart::is_enabled();
                        if let Err(e) = autostart::set(target) {
                            tracing::error!(error = %e, "autostart toggle failed");
                        }
                        autostart_i.set_checked(autostart::is_enabled());
                    }
                    id if id == autoupdate_i.id() => {
                        // Same opt-in toggle pattern as autostart: flip the persisted
                        // pref, re-sync the checkmark to reality, and run a check
                        // immediately when turning it on so it gives feedback.
                        let target = !prefs::auto_update_enabled();
                        if let Err(e) = prefs::set_auto_update(target) {
                            tracing::error!(error = %e, "auto-update toggle failed");
                        }
                        let on = prefs::auto_update_enabled();
                        autoupdate_i.set_checked(on);
                        if on {
                            toast::toast("runic — updates", "checking…");
                            spawn_update_check(supervisor.rt.handle(), toast_proxy.clone(), true);
                        }
                    }
                    id if id == quit_i.id() => {
                        supervisor.stop();
                        *control_flow = ControlFlow::Exit;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    });
}
