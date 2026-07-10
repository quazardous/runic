//! Loopback admin control plane — a minimal hand-rolled HTTP/1.1 + JSON server.
//!
//! Resource-per-name REST with a `?permanent=true` toggle — shape borrowed from
//! gost's admin API (`POST/PUT/DELETE /config/<section>/<name>` + its `?save`
//! flag). The runtime/permanent split itself follows firewalld; see
//! [`crate::store`].
//!
//! No framework (no hyper/axum): the surface is ~9 routes on a trusted loopback
//! control plane, so a tiny request reader + a `match` router keeps the binary
//! lean (consistent with the hand-rolled SOCKS5 data plane). Connections are
//! one-shot (`Connection: close`); no keep-alive, no pipelining.
//!
//! Trust boundary = the bind address (loopback, no auth), same stance as the
//! SOCKS5 surface. Handlers never panic — every error path returns a JSON body.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::config::{Config, SiloAuth, UpstreamKind, UpstreamSpec, DEFAULT_UPSTREAM_NAME};
use crate::filter::FilterRules;
use crate::server::SiloPorts;
use crate::silo::{VariationCache, VariationData};
use crate::stats::Stats;
use crate::store::ConfigStore;

/// Everything the admin API needs to serve silo requests: the variation cache,
/// the `none`-mode port registry, and the instance default binding mode.
#[derive(Clone)]
pub struct SiloAdmin {
    pub cache: Arc<Mutex<VariationCache>>,
    pub ports: Arc<SiloPorts>,
    pub default_mode: SiloAuth,
}

const REQUEST_HEAD_LIMIT: usize = 16 * 1024;
const REQUEST_BODY_LIMIT: usize = 256 * 1024;

/// Bind the admin listener and spawn its accept loop. Returns the bound address
/// once the bind succeeds so the caller can fail fast on a port clash (and tests
/// can target an OS-assigned `:0` port).
pub async fn spawn(
    addr: SocketAddr,
    store: Arc<Mutex<ConfigStore>>,
    silo: Option<SiloAdmin>,
    stats: Arc<Stats>,
) -> Result<SocketAddr> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind admin API on {addr}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    let started = Instant::now();
    info!(addr = %local, "admin API listening");
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((sock, peer)) => {
                    let store = store.clone();
                    let silo = silo.clone();
                    let stats = stats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(sock, store, silo, stats, started).await {
                            debug!(%peer, error = %e, "admin connection error");
                        }
                    });
                }
                Err(e) => error!(?e, "admin accept failed"),
            }
        }
    });
    Ok(local)
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
    /// Token from an `Authorization: Bearer <token>` header, if present.
    bearer: Option<String>,
}

async fn handle_conn(
    mut sock: TcpStream,
    store: Arc<Mutex<ConfigStore>>,
    silo: Option<SiloAdmin>,
    stats: Arc<Stats>,
    started: Instant,
) -> Result<()> {
    let req = match read_request(&mut sock).await {
        Ok(req) => req,
        Err(e) => {
            let resp = json_response(400, "Bad Request", &json!({ "error": e.to_string() }));
            sock.write_all(&resp).await.ok();
            return Ok(());
        }
    };

    let resp = route(&req, &store, &silo, &stats, started).await;
    sock.write_all(&resp)
        .await
        .context("write admin response")?;
    sock.flush().await.ok();
    Ok(())
}

async fn route(
    req: &Request,
    store: &Arc<Mutex<ConfigStore>>,
    silo: &Option<SiloAdmin>,
    stats: &Arc<Stats>,
    started: Instant,
) -> Vec<u8> {
    let permanent = query_flag(&req.query, "permanent");

    match (req.method.as_str(), req.path.as_str()) {
        // The human-facing status page (self-contained HTML+JS, consumes
        // `/v1/status`). Served on the same loopback admin port.
        ("GET", "/") | ("GET", "/status") | ("GET", "/index.html") => {
            html_response(200, "OK", include_str!("ui/status.html"))
        }
        ("GET", "/v1/status") => status_response(store, silo, stats, started).await,
        ("GET", "/v1/config") => {
            // Silo mode + Bearer ⇒ show that variation's own config (the warm-boot
            // "is my pool already populated?" check), not the global store.
            if let (Some(silo), Some(token)) = (silo.as_ref(), req.bearer.as_ref()) {
                return match silo.cache.lock().await.access(token, unix_now()) {
                    Ok(data) => json_response(200, "OK", &json!({ "upstreams": data.upstreams })),
                    Err(_) => {
                        json_response(404, "Not Found", &json!({ "code": "silo_token_unknown" }))
                    }
                };
            }
            let cfg = store.lock().await.merged();
            json_response(
                200,
                "OK",
                &json!({
                    "listen": { "addr": cfg.listen.addr.to_string() },
                    "upstreams": cfg.upstreams,
                    "active_route": cfg.active_route,
                }),
            )
        }
        ("GET", "/v1/diagnose") => {
            let diag = store.lock().await.diagnose();
            json_response(200, "OK", &json!(diag))
        }
        ("GET", "/v1/diff") => {
            let diff = store.lock().await.diff();
            json_response(200, "OK", &json!(diff))
        }
        ("POST", "/v1/snapshot/promote") => {
            let mut store = store.lock().await;
            match store.promote_runtime_to_permanent() {
                Ok(n) => json_response(200, "OK", &json!({ "promoted": n })),
                Err(e) => persist_error(e),
            }
        }
        ("DELETE", "/v1/snapshot") => {
            let mut store = store.lock().await;
            match store.wipe_snapshot() {
                Ok(()) => json_response(200, "OK", &json!({ "wiped": true })),
                Err(e) => persist_error(e),
            }
        }
        ("PUT", "/v1/route/default") => {
            // Point the no-provider ("default") route at a named upstream, live.
            // Switching by name — the upstream's creds stay put, not re-sent.
            let name = match serde_json::from_slice::<serde_json::Value>(&req.body)
                .ok()
                .and_then(|v| v.get("upstream").and_then(|x| x.as_str()).map(String::from))
            {
                Some(n) => n,
                None => {
                    return json_response(
                        400,
                        "Bad Request",
                        &json!({ "error": "body must be {\"upstream\":\"<name>\"}" }),
                    )
                }
            };
            let mut store = store.lock().await;
            if !store.merged().upstreams.contains_key(&name) {
                return json_response(
                    404,
                    "Not Found",
                    &json!({ "error": format!("no upstream '{name}' to point the default route at") }),
                );
            }
            store.set_active_route(Some(name.clone()));
            json_response(200, "OK", &json!({ "active_route": name }))
        }
        ("DELETE", "/v1/route/default") => {
            // Clear the pointer: the default route falls back to the `default` entry.
            store.lock().await.set_active_route(None);
            json_response(
                200,
                "OK",
                &json!({ "active_route": serde_json::Value::Null }),
            )
        }
        ("GET", "/v1/filter") => {
            // Silo mode + Bearer ⇒ that variation's own filter; otherwise the
            // effective global filter (hot ▷ snapshot ▷ cold).
            if let (Some(silo), Some(token)) = (silo.as_ref(), req.bearer.as_ref()) {
                return match silo.cache.lock().await.access(token, unix_now()) {
                    Ok(data) => json_response(200, "OK", &json!(data.filter)),
                    Err(_) => {
                        json_response(404, "Not Found", &json!({ "code": "silo_token_unknown" }))
                    }
                };
            }
            let filter = store.lock().await.effective_filter();
            json_response(200, "OK", &json!(filter))
        }
        ("PUT", "/v1/filter") => {
            let filter: FilterRules = match serde_json::from_slice(&req.body) {
                Ok(f) => f,
                Err(e) => {
                    return json_response(
                        400,
                        "Bad Request",
                        &json!({ "error": format!("invalid filter body: {e}") }),
                    )
                }
            };
            // Silo mode + Bearer ⇒ write-through into that variation's encrypted
            // config (the blob *is* the persistence — `?permanent` is moot).
            if let (Some(silo), Some(token)) = (silo.as_ref(), req.bearer.as_ref()) {
                let now = unix_now();
                let mut cache = silo.cache.lock().await;
                let mut data = match cache.access(token, now) {
                    Ok(d) => d,
                    Err(_) => {
                        return json_response(
                            404,
                            "Not Found",
                            &json!({ "code": "silo_token_unknown" }),
                        )
                    }
                };
                data.filter = filter;
                if let Err(e) = cache.write(token, &data, now) {
                    return json_response(
                        500,
                        "Internal Server Error",
                        &json!({ "error": e.to_string() }),
                    );
                }
                return json_response(200, "OK", &json!({ "silo": true }));
            }
            let mut store = store.lock().await;
            if permanent {
                if let Err(e) = store.set_filter_permanent(filter) {
                    return persist_error(e);
                }
            } else {
                store.set_filter_runtime(filter);
            }
            json_response(200, "OK", &json!({ "permanent": permanent }))
        }
        ("DELETE", "/v1/filter") => {
            // Silo mode + Bearer ⇒ clear that variation's own filter.
            if let (Some(silo), Some(token)) = (silo.as_ref(), req.bearer.as_ref()) {
                let now = unix_now();
                let mut cache = silo.cache.lock().await;
                let mut data = match cache.access(token, now) {
                    Ok(d) => d,
                    Err(_) => {
                        return json_response(
                            404,
                            "Not Found",
                            &json!({ "code": "silo_token_unknown" }),
                        )
                    }
                };
                data.filter = FilterRules::default();
                if let Err(e) = cache.write(token, &data, now) {
                    return json_response(
                        500,
                        "Internal Server Error",
                        &json!({ "error": e.to_string() }),
                    );
                }
                return json_response(200, "OK", &json!({ "silo": true, "cleared": true }));
            }
            let mut store = store.lock().await;
            let cleared = if permanent {
                match store.clear_filter_permanent() {
                    Ok(c) => c,
                    Err(e) => return persist_error(e),
                }
            } else {
                store.clear_filter_runtime()
            };
            json_response(
                200,
                "OK",
                &json!({ "cleared": cleared, "permanent": permanent }),
            )
        }
        ("POST", "/v1/silo/open") => silo_open(req, silo).await,
        ("POST", path) if path.starts_with("/v1/upstreams/") => {
            let name = match upstream_name(path) {
                Some(n) => n,
                None => {
                    return json_response(
                        400,
                        "Bad Request",
                        &json!({ "error": "missing upstream name" }),
                    )
                }
            };
            let spec: UpstreamSpec = match serde_json::from_slice(&req.body) {
                Ok(s) => s,
                Err(e) => {
                    return json_response(
                        400,
                        "Bad Request",
                        &json!({ "error": format!("invalid upstream body: {e}") }),
                    )
                }
            };
            let up = match spec.resolve(&name) {
                Ok(up) => up,
                Err(e) => {
                    return json_response(400, "Bad Request", &json!({ "error": e.to_string() }))
                }
            };
            // Silo mode + a Bearer token ⇒ write-through into that variation's
            // encrypted config (the blob *is* the persistence — `?permanent` moot).
            if let (Some(silo), Some(token)) = (silo.as_ref(), req.bearer.as_ref()) {
                let now = unix_now();
                let mut cache = silo.cache.lock().await;
                let mut data = match cache.access(token, now) {
                    Ok(d) => d,
                    Err(_) => {
                        return json_response(
                            404,
                            "Not Found",
                            &json!({ "code": "silo_token_unknown" }),
                        )
                    }
                };
                data.upstreams.insert(name.clone(), up);
                if let Err(e) = cache.write(token, &data, now) {
                    return json_response(
                        500,
                        "Internal Server Error",
                        &json!({ "error": e.to_string() }),
                    );
                }
                return json_response(200, "OK", &json!({ "name": name, "silo": true }));
            }
            let mut store = store.lock().await;
            if permanent {
                if let Err(e) = store.apply_permanent(name.clone(), up) {
                    return persist_error(e);
                }
            } else {
                store.apply_runtime(name.clone(), up);
            }
            let source = store.diagnose().get(&name).copied();
            json_response(
                200,
                "OK",
                &json!({ "name": name, "permanent": permanent, "source": source }),
            )
        }
        ("DELETE", path) if path.starts_with("/v1/upstreams/") => {
            let name = match upstream_name(path) {
                Some(n) => n,
                None => {
                    return json_response(
                        400,
                        "Bad Request",
                        &json!({ "error": "missing upstream name" }),
                    )
                }
            };
            let mut store = store.lock().await;
            let removed = if permanent {
                match store.remove_permanent(&name) {
                    Ok(r) => r,
                    Err(e) => return persist_error(e),
                }
            } else {
                store.remove_runtime(&name)
            };
            if !removed {
                return json_response(
                    404,
                    "Not Found",
                    &json!({ "error": format!("no runtime/snapshot entry '{name}'") }),
                );
            }
            let fallback = store.diagnose().get(&name).copied();
            json_response(
                200,
                "OK",
                &json!({ "name": name, "removed": true, "permanent": permanent, "now": fallback }),
            )
        }
        _ => json_response(404, "Not Found", &json!({ "error": "no such route" })),
    }
}

fn persist_error(e: anyhow::Error) -> Vec<u8> {
    warn!(error = %e, "admin snapshot persistence failed");
    json_response(
        500,
        "Internal Server Error",
        &json!({ "error": e.to_string() }),
    )
}

/// `POST /v1/silo/open` — the one silo verb (the "variation" notion stays
/// internal). No `Authorization` → mint a fresh silo token and return it once.
/// `Authorization: Bearer <token>` → "patte blanche": confirm the token opens a
/// live variation; an unknown/purged token gets a **distinct** `404
/// silo_token_unknown` (never a silent mint — the client re-opens without auth).
async fn silo_open(req: &Request, silo: &Option<SiloAdmin>) -> Vec<u8> {
    let Some(silo) = silo else {
        return json_response(
            404,
            "Not Found",
            &json!({ "error": "silo mode not enabled" }),
        );
    };
    // An optional {"mode":"rfc1929"|"none"} body overrides the instance default.
    let mode = parse_silo_mode(&req.body).unwrap_or(silo.default_mode);
    let now = unix_now();

    // No Bearer ⇒ mint a fresh token; Bearer ⇒ use the presented one.
    let minted = req.bearer.is_none();
    let token = match &req.bearer {
        Some(t) => t.clone(),
        None => match silo.cache.lock().await.create(now) {
            Ok(t) => t,
            Err(e) => {
                return json_response(
                    500,
                    "Internal Server Error",
                    &json!({ "error": e.to_string() }),
                )
            }
        },
    };

    // Warm the variation (decrypt into RAM). For a Bearer token, an error here is
    // the deterministic orphan signal — never a silent re-mint.
    if silo.cache.lock().await.access(&token, now).is_err() {
        return json_response(404, "Not Found", &json!({ "code": "silo_token_unknown" }));
    }

    // `none` mode → bind (or reuse) a dedicated no-auth loopback port.
    let port = if mode == SiloAuth::None {
        let Some(id) = VariationCache::id_of(&token) else {
            return json_response(400, "Bad Request", &json!({ "error": "bad token" }));
        };
        match silo.ports.ensure(&id).await {
            Ok(p) => Some(p),
            Err(e) => {
                return json_response(
                    500,
                    "Internal Server Error",
                    &json!({ "error": e.to_string() }),
                )
            }
        }
    } else {
        None
    };

    match (minted, port) {
        (true, Some(p)) => json_response(200, "OK", &json!({ "token": token, "port": p })),
        (true, None) => json_response(200, "OK", &json!({ "token": token })),
        (false, Some(p)) => json_response(200, "OK", &json!({ "port": p })),
        (false, None) => json_response(200, "OK", &json!({ "ok": true })),
    }
}

/// Parse an optional `{"mode":"rfc1929"|"none"}` body. `None` ⇒ use the default.
fn parse_silo_mode(body: &[u8]) -> Option<SiloAuth> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    match v.get("mode")?.as_str()? {
        "none" => Some(SiloAuth::None),
        "rfc1929" => Some(SiloAuth::Rfc1929),
        _ => None,
    }
}

/// Current unix time in seconds (the silo clock for create/access/TTL).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Extract the `<name>` from `/v1/upstreams/<name>` (URL-decoded minimally).
fn upstream_name(path: &str) -> Option<String> {
    let raw = path.strip_prefix("/v1/upstreams/")?;
    if raw.is_empty() || raw.contains('/') {
        return None;
    }
    Some(raw.to_string())
}

/// Truthy check for a query flag like `permanent` / `permanent=true`.
fn query_flag(query: &str, key: &str) -> bool {
    query.split('&').any(|kv| {
        let mut it = kv.splitn(2, '=');
        match (it.next(), it.next()) {
            (Some(k), Some(v)) => k == key && (v == "true" || v == "1"),
            (Some(k), None) => k == key,
            _ => false,
        }
    })
}

fn json_response(code: u16, reason: &str, body: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&body);
    out
}

fn html_response(code: u16, reason: &str, body: &str) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Host machine name, detected once per process. Lets a fleet consumer (or a
/// human with several runic boxes) tell instances apart from the status API
/// alone. Falls back to `"unknown"` when the OS returns nothing usable.
fn host_name() -> &'static str {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let name = gethostname::gethostname().to_string_lossy().into_owned();
        if name.is_empty() {
            "unknown".to_string()
        } else {
            name
        }
    })
}

/// Build the enriched `GET /v1/status` body: the **live runtime view** —
/// version, uptime, listen, the active route, the hot upstream pool, live session
/// counters, and the silo variations with per-variation connections/requests. All
/// from cleartext index metadata + RAM counters: **no token, no decryption**.
async fn status_response(
    store: &Arc<Mutex<ConfigStore>>,
    silo: &Option<SiloAdmin>,
    stats: &Arc<Stats>,
    started: Instant,
) -> Vec<u8> {
    let snap = stats.snapshot();
    let (merged, hot) = {
        let s = store.lock().await;
        (s.merged(), s.hot().clone())
    };

    let active_route =
        route_named(&merged).map(|(name, kind)| json!({ "name": name, "kind": kind }));

    // Compact view of the effective global filter (the full ruleset is at
    // `GET /v1/filter`). `active` = at least one rule or a non-allow default.
    // The instance filter (non-silo sessions); `silo_floor` is the static file
    // baseline each silo composes on top of.
    let filter = &merged.filter;
    let filter_json = json!({
        "active": !filter.is_noop(),
        "default": filter.default,
        "rules": filter.rules.len(),
        "silo_floor_rules": merged.silo_floor_filter.rules.len(),
    });

    let upstreams_hot: Vec<_> = hot
        .iter()
        .map(|(name, up)| json!({ "name": name, "kind": up.kind }))
        .collect();

    let silo_json = match silo {
        None => json!({ "enabled": false }),
        Some(sa) => {
            let now = unix_now();
            let cache = sa.cache.lock().await;
            let ttl = cache.store().ttl_secs();
            let variations: Vec<_> = cache
                .store()
                .variations_meta()
                .into_iter()
                .map(|m| {
                    let warm = cache.is_warm(&m.id);
                    // Per-variation route (name + kind) is only knowable while the
                    // variation is warm (its pool is decrypted in RAM); a cold
                    // variation has no live sessions, so it cannot be leaking.
                    let route = if warm {
                        cache
                            .warm_data(&m.id)
                            .and_then(|d| variation_route(&merged, &d))
                            .map(|(name, kind)| json!({ "name": name, "kind": kind }))
                    } else {
                        None
                    };
                    let vs = snap.variation(&m.id);
                    let idle = now.saturating_sub(m.last_access);
                    json!({
                        "id": short_id(&m.id),
                        "warm": warm,
                        "route": route,
                        "connections": vs.active,
                        "requests": vs.requests,
                        "filtered": vs.filtered,
                        "last_access_secs": idle,
                        "ttl_secs_remaining": ttl.saturating_sub(idle),
                    })
                })
                .collect();
            json!({
                "enabled": true,
                "auth": silo_auth_str(sa.default_mode),
                "variations": variations,
            })
        }
    };

    json_response(
        200,
        "OK",
        &json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "hostname": host_name(),
            "uptime_secs": started.elapsed().as_secs(),
            // Kept for back-compat (existing consumers read it); counts the merged
            // effective pool. The new fields below are the live runtime view.
            "pool_size": merged.upstreams.len(),
            // The *actually bound* SOCKS5 address. In auto-port mode
            // (`listen.addr` with port 0) this is the discovery contract: the
            // fixed admin port is the rendezvous, a client reads the real
            // SOCKS5 port here. Falls back to the configured address until
            // the listener has bound; `listen_configured` keeps the raw
            // config value for audit.
            "listen": snap.bound_addr.map(|a| a.to_string())
                .unwrap_or_else(|| merged.listen.addr.to_string()),
            "listen_configured": merged.listen.addr.to_string(),
            "active_route": active_route,
            // Conservative leak signal for the tray icon: true iff ≥1 active
            // session is routing through a `direct` upstream (local IP exposed).
            "any_active_direct": snap.any_active_direct(),
            "active_sessions": snap.active_total,
            "requests_total": snap.requests_total,
            // Cumulative CONNECTs refused by the domain filter.
            "filtered_total": snap.filtered_total,
            "filter": filter_json,
            "upstreams_hot": upstreams_hot,
            "silo": silo_json,
        }),
    )
}

/// The no-hint ("default") route's `(name, kind)` for a config — mirrors
/// [`crate::routing::pick_upstream`]`(cfg, None)`: the active-route pointer if it
/// resolves, else the entry named `default`, else `None` (empty / default-less).
fn route_named(cfg: &Config) -> Option<(String, UpstreamKind)> {
    let name = cfg
        .active_route
        .as_ref()
        .filter(|n| cfg.upstreams.contains_key(n.as_str()))
        .cloned()
        .or_else(|| {
            cfg.upstreams
                .contains_key(DEFAULT_UPSTREAM_NAME)
                .then(|| DEFAULT_UPSTREAM_NAME.to_string())
        })?;
    let kind = cfg.upstreams.get(&name)?.kind;
    Some((name, kind))
}

/// The route a silo variation takes for a no-hint session: resolved over the
/// **hot/cold pool ∪ the variation's own pool** (the variation wins on a clash),
/// same overlay the data plane's `pick_from_merged` uses.
fn variation_route(merged: &Config, data: &VariationData) -> Option<(String, UpstreamKind)> {
    let mut pool = merged.upstreams.clone();
    pool.extend(data.upstreams.clone());
    let cfg = Config {
        listen: merged.listen.clone(),
        upstreams: pool,
        active_route: merged.active_route.clone(),
        filter: merged.filter.clone(),
        silo_floor_filter: merged.silo_floor_filter.clone(),
    };
    route_named(&cfg)
}

fn silo_auth_str(mode: SiloAuth) -> &'static str {
    match mode {
        SiloAuth::Rfc1929 => "rfc1929",
        SiloAuth::None => "none",
    }
}

/// Short, display-friendly form of a variation id (the full id is `SHA256(token)`
/// hex; 12 hex chars is plenty to tell variations apart on the status page).
fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

async fn read_request(sock: &mut TcpStream) -> Result<Request> {
    // Read until end of headers.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > REQUEST_HEAD_LIMIT {
            return Err(anyhow!("request headers exceed {REQUEST_HEAD_LIMIT} bytes"));
        }
        let n = sock.read(&mut chunk).await.context("read admin request")?;
        if n == 0 {
            return Err(anyhow!("connection closed before request complete"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head =
        std::str::from_utf8(&buf[..header_end]).map_err(|_| anyhow!("request head not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or_else(|| anyhow!("empty request"))?;
    let mut parts = request_line.split(' ');
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("missing method"))?
        .to_string();
    let target = parts.next().ok_or_else(|| anyhow!("missing target"))?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut content_length = 0usize;
    let mut bearer = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("authorization") {
                let v = v.trim();
                if let Some(tok) = v
                    .strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
                {
                    bearer = Some(tok.trim().to_string());
                }
            }
        }
    }
    if content_length > REQUEST_BODY_LIMIT {
        return Err(anyhow!("request body exceeds {REQUEST_BODY_LIMIT} bytes"));
    }

    // Body bytes already read past the header terminator, plus whatever's left.
    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = sock
            .read(&mut chunk)
            .await
            .context("read admin request body")?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        query,
        body,
        bearer,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Listen, ListenAuth};
    use crate::store::ConfigStore;
    use std::collections::BTreeMap;

    async fn test_server() -> (SocketAddr, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut upstreams = BTreeMap::new();
        upstreams.insert(
            "default".to_string(),
            crate::config::Upstream {
                kind: crate::config::UpstreamKind::HttpConnect,
                host: "cold.example".into(),
                port: 823,
                auth: crate::config::UpstreamCreds {
                    username: "u".into(),
                    password: "p".into(),
                },
            },
        );
        let cold = Config {
            listen: Listen {
                addr: "127.0.0.1:0".parse().unwrap(),
                port_range: None,
                auth: ListenAuth::None,
            },
            upstreams,
            active_route: None,
            filter: crate::filter::FilterRules::default(),
            silo_floor_filter: crate::filter::FilterRules::default(),
        };
        let (store, _rx) = ConfigStore::new(cold, dir.path().join("runic.snapshot.json"));
        let store = Arc::new(Mutex::new(store));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let started = Instant::now();
        let stats = crate::stats::Stats::new();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let store = store.clone();
                let stats = stats.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, store, None, stats, started).await;
                });
            }
        });
        (addr, dir)
    }

    /// Like [`test_server`] but with silo mode enabled (an empty `VariationCache`).
    async fn test_server_with_silo(default_mode: SiloAuth) -> (SocketAddr, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cold = Config {
            listen: Listen {
                addr: "127.0.0.1:0".parse().unwrap(),
                port_range: None,
                auth: ListenAuth::None,
            },
            upstreams: BTreeMap::new(),
            active_route: None,
            filter: crate::filter::FilterRules::default(),
            silo_floor_filter: crate::filter::FilterRules::default(),
        };
        let (store, cfg_rx) = ConfigStore::new(cold, dir.path().join("runic.snapshot.json"));
        let store = Arc::new(Mutex::new(store));
        let cache = Arc::new(Mutex::new(crate::silo::VariationCache::new(
            crate::silo::SiloStore::open(dir.path().join("runic.silo"), 3600).unwrap(),
            3600,
        )));
        let stats = crate::stats::Stats::new();
        let ports = SiloPorts::new(cfg_rx, cache.clone(), stats.clone());
        let silo = Some(SiloAdmin {
            cache,
            ports,
            default_mode,
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let started = Instant::now();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let store = store.clone();
                let silo = silo.clone();
                let stats = stats.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, store, silo, stats, started).await;
                });
            }
        });
        (addr, dir)
    }

    fn post_silo_open(bearer: Option<&str>) -> String {
        let auth = bearer
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        format!("POST /v1/silo/open HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")
    }

    async fn http(addr: SocketAddr, raw: &str) -> (u16, String) {
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(raw.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        sock.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp).to_string();
        let code: u16 = text
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (code, body)
    }

    fn post_upstream(name: &str, host: &str, permanent: bool) -> String {
        let body = format!(
            r#"{{"kind":"http_connect","host":"{host}","port":823,"auth":{{"username":"x","password":"y"}}}}"#
        );
        let q = if permanent { "?permanent=true" } else { "" };
        format!(
            "POST /v1/upstreams/{name}{q} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn status_returns_pool_size() {
        let (addr, _dir) = test_server().await;
        let (code, body) = http(
            addr,
            "GET /v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(body.contains("\"pool_size\":1"), "got: {body}");
        assert!(body.contains("\"status\":\"ok\""), "got: {body}");
    }

    #[tokio::test]
    async fn status_exposes_live_runtime_fields() {
        let (addr, _dir) = test_server().await;
        let (code, body) = http(
            addr,
            "GET /v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        // The cold `default` upstream is the resolved active route (http_connect).
        assert!(body.contains("\"active_route\""), "got: {body}");
        assert!(body.contains("\"name\":\"default\""), "got: {body}");
        assert!(body.contains("\"kind\":\"http_connect\""), "got: {body}");
        // Live counters, fresh instance.
        assert!(body.contains("\"any_active_direct\":false"), "got: {body}");
        assert!(body.contains("\"active_sessions\":0"), "got: {body}");
        assert!(body.contains("\"requests_total\":0"), "got: {body}");
        // No upstream has been pushed at runtime → hot layer is empty.
        assert!(body.contains("\"upstreams_hot\":[]"), "got: {body}");
        // No silo on this instance.
        assert!(body.contains("\"silo\":{\"enabled\":false}"), "got: {body}");
        // Host machine name is always present and non-empty.
        assert!(body.contains("\"hostname\":\""), "got: {body}");
        assert!(!body.contains("\"hostname\":\"\""), "got: {body}");
        // No SOCKS5 server runs in this harness → no bound addr published, so
        // `listen` falls back to the configured address, kept in
        // `listen_configured` for audit.
        assert!(body.contains("\"listen_configured\":\""), "got: {body}");
    }

    #[tokio::test]
    async fn filter_put_get_delete_roundtrip() {
        let (addr, _dir) = test_server().await;

        // Fresh instance: no filter configured.
        let (code, body) = http(
            addr,
            "GET /v1/filter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(body.contains("\"rules\":[]"), "got: {body}");

        // PUT a blocklist at runtime.
        let put_body =
            r#"{"default":"allow","rules":[{"deny":"*.doubleclick.net"},{"allow":"cdn.ok.com"}]}"#;
        let (code, _b) = http(
            addr,
            &format!(
                "PUT /v1/filter HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{put_body}",
                put_body.len()
            ),
        )
        .await;
        assert_eq!(code, 200);

        // GET it back.
        let (code, body) = http(
            addr,
            "GET /v1/filter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(
            body.contains("\"deny\":\"*.doubleclick.net\""),
            "got: {body}"
        );
        assert!(body.contains("\"allow\":\"cdn.ok.com\""), "got: {body}");

        // The status page reflects the active filter (2 rules, blocklist).
        let (_c, sbody) = http(
            addr,
            "GET /v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(sbody.contains("\"filtered_total\":0"), "got: {sbody}");
        assert!(sbody.contains("\"active\":true"), "got: {sbody}");
        assert!(sbody.contains("\"rules\":2"), "got: {sbody}");

        // DELETE clears it.
        let (code, body) = http(
            addr,
            "DELETE /v1/filter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(body.contains("\"cleared\":true"), "got: {body}");

        let (_c, body) = http(
            addr,
            "GET /v1/filter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(body.contains("\"rules\":[]"), "got: {body}");
    }

    #[tokio::test]
    async fn filter_rejects_rule_with_both_actions() {
        let (addr, _dir) = test_server().await;
        let bad = r#"{"rules":[{"deny":"x.com","allow":"x.com"}]}"#;
        let (code, _b) = http(
            addr,
            &format!(
                "PUT /v1/filter HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bad}",
                bad.len()
            ),
        )
        .await;
        assert_eq!(code, 400);
    }

    #[tokio::test]
    async fn filter_rejects_bare_ipv6_pattern_with_400() {
        // A bare IPv6 literal would silently misparse (trailing `:<n>` read as
        // a port) — the API must refuse it loudly and point at the bracket form.
        let (addr, _dir) = test_server().await;
        let bad = r#"{"rules":[{"deny":"2001:db8::1"}]}"#;
        let (code, body) = http(
            addr,
            &format!(
                "PUT /v1/filter HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bad}",
                bad.len()
            ),
        )
        .await;
        assert_eq!(code, 400);
        assert!(body.contains("[2001:db8::1]"), "got: {body}");

        // The bracketed form is accepted.
        let good = r#"{"rules":[{"deny":"[2001:db8::1]"}]}"#;
        let (code, _b) = http(
            addr,
            &format!(
                "PUT /v1/filter HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{good}",
                good.len()
            ),
        )
        .await;
        assert_eq!(code, 200);
    }

    #[tokio::test]
    async fn serves_self_contained_html_status_page() {
        let (addr, _dir) = test_server().await;
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        sock.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "got: {text}");
        assert!(
            text.contains("Content-Type: text/html"),
            "expected html content-type"
        );
        // Self-contained: the page references the API and carries no external asset.
        assert!(text.contains("/v1/status"), "page should consume the API");
        assert!(!text.contains("http://") && !text.contains("https://"));
    }

    #[tokio::test]
    async fn status_lists_silo_variations_without_token() {
        let (addr, _d) = test_server_with_silo(SiloAuth::None).await;
        // Open a variation (none-mode → mints a token + warms it).
        let (code, body) = http(addr, &post_silo_open(None)).await;
        assert_eq!(code, 200, "got: {body}");

        let (code, body) = http(
            addr,
            "GET /v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(body.contains("\"enabled\":true"), "got: {body}");
        assert!(body.contains("\"auth\":\"none\""), "got: {body}");
        // The warm variation is listed with zeroed live counters — no token, no
        // decryption needed to report it.
        assert!(body.contains("\"warm\":true"), "got: {body}");
        assert!(body.contains("\"connections\":0"), "got: {body}");
        assert!(body.contains("\"requests\":0"), "got: {body}");
    }

    #[tokio::test]
    async fn post_upstream_runtime_only_then_diagnose_hot() {
        let (addr, _dir) = test_server().await;
        let (code, _) = http(addr, &post_upstream("us", "us.example", false)).await;
        assert_eq!(code, 200);

        let (_, body) = http(
            addr,
            "GET /v1/diagnose HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(body.contains("\"us\":\"hot\""), "got: {body}");
    }

    #[tokio::test]
    async fn post_upstream_permanent_persists_to_snapshot_file() {
        let (addr, dir) = test_server().await;
        let (code, body) = http(addr, &post_upstream("us", "us.example", true)).await;
        assert_eq!(code, 200, "got: {body}");
        assert!(body.contains("\"source\":\"snapshot\""), "got: {body}");
        assert!(dir.path().join("runic.snapshot.json").exists());
    }

    #[tokio::test]
    async fn delete_runtime_falls_back_to_snapshot() {
        let (addr, _dir) = test_server().await;
        http(addr, &post_upstream("us", "snap.example", true)).await;
        http(addr, &post_upstream("us", "hot.example", false)).await;
        let (code, body) = http(
            addr,
            "DELETE /v1/upstreams/us HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(body.contains("\"now\":\"snapshot\""), "got: {body}");
    }

    #[tokio::test]
    async fn delete_unknown_upstream_404() {
        let (addr, _dir) = test_server().await;
        let (code, _) = http(
            addr,
            "DELETE /v1/upstreams/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 404);
    }

    #[tokio::test]
    async fn invalid_json_body_400() {
        let (addr, _dir) = test_server().await;
        let raw = "POST /v1/upstreams/us HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nConnection: close\r\n\r\n{bad}";
        let (code, _) = http(addr, raw).await;
        assert_eq!(code, 400);
    }

    #[tokio::test]
    async fn unknown_route_404() {
        let (addr, _dir) = test_server().await;
        let (code, _) = http(
            addr,
            "GET /v1/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 404);
    }

    // --- silo open ----------------------------------------------------------

    #[tokio::test]
    async fn silo_open_mints_token() {
        let (addr, _d) = test_server_with_silo(SiloAuth::Rfc1929).await;
        let (code, body) = http(addr, &post_silo_open(None)).await;
        assert_eq!(code, 200, "got: {body}");
        assert!(body.contains("\"token\""), "got: {body}");
    }

    #[tokio::test]
    async fn silo_open_with_valid_token_ok() {
        let (addr, _d) = test_server_with_silo(SiloAuth::Rfc1929).await;
        let (_c, minted) = http(addr, &post_silo_open(None)).await;
        let token = minted
            .split("\"token\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("token in mint response")
            .to_string();

        let (code, body) = http(addr, &post_silo_open(Some(&token))).await;
        assert_eq!(code, 200, "got: {body}");
        assert!(body.contains("\"ok\":true"), "got: {body}");
    }

    #[tokio::test]
    async fn silo_open_unknown_token_is_404_silo_token_unknown() {
        let (addr, _d) = test_server_with_silo(SiloAuth::Rfc1929).await;
        let (code, body) = http(addr, &post_silo_open(Some("not-a-real-token"))).await;
        assert_eq!(code, 404);
        assert!(body.contains("silo_token_unknown"), "got: {body}");
    }

    #[tokio::test]
    async fn silo_open_404_when_silo_disabled() {
        let (addr, _d) = test_server().await; // no silo
        let (code, _) = http(addr, &post_silo_open(None)).await;
        assert_eq!(code, 404);
    }

    #[tokio::test]
    async fn silo_open_none_mode_returns_port_idempotent() {
        let (addr, _d) = test_server_with_silo(SiloAuth::None).await;

        // Mint in `none` mode → {token, port}.
        let (code, body) = http(addr, &post_silo_open(None)).await;
        assert_eq!(code, 200, "got: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let token = v["token"].as_str().expect("token").to_string();
        let port1 = v["port"].as_u64().expect("port");
        assert!(port1 > 0);

        // Re-open the same token while warm → the SAME port (idempotent).
        let (code2, body2) = http(addr, &post_silo_open(Some(&token))).await;
        assert_eq!(code2, 200, "got: {body2}");
        let v2: serde_json::Value = serde_json::from_str(&body2).unwrap();
        assert_eq!(
            v2["port"].as_u64().expect("port"),
            port1,
            "same port while warm"
        );
    }

    #[tokio::test]
    async fn silo_bearer_upstream_writes_through_and_config_shows_it() {
        let (addr, _d) = test_server_with_silo(SiloAuth::Rfc1929).await;
        let (_c, body) = http(addr, &post_silo_open(None)).await;
        let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();

        // Push an upstream into *my* variation (Bearer-scoped, write-through).
        let up = r#"{"kind":"http_connect","host":"v.example","port":823,"auth":{"username":"u","password":"p"}}"#;
        let raw = format!(
            "POST /v1/upstreams/default HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{up}",
            up.len()
        );
        let (code, body2) = http(addr, &raw).await;
        assert_eq!(code, 200, "got: {body2}");
        assert!(body2.contains("\"silo\":true"), "got: {body2}");

        // GET /v1/config with the same token shows my (now populated) pool.
        let raw_g = format!(
            "GET /v1/config HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        );
        let (code3, cfg) = http(addr, &raw_g).await;
        assert_eq!(code3, 200);
        assert!(cfg.contains("v.example"), "got: {cfg}");
    }

    fn put_route(name: &str) -> String {
        let body = format!(r#"{{"upstream":"{name}"}}"#);
        format!(
            "PUT /v1/route/default HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn put_route_default_sets_active_route() {
        let (addr, _dir) = test_server().await;
        http(addr, &post_upstream("us", "us.example", false)).await;

        let (code, resp) = http(addr, &put_route("us")).await;
        assert_eq!(code, 200, "got: {resp}");
        assert!(resp.contains("\"active_route\":\"us\""), "got: {resp}");

        let (_, cfg) = http(
            addr,
            "GET /v1/config HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(cfg.contains("\"active_route\":\"us\""), "got: {cfg}");
    }

    #[tokio::test]
    async fn put_route_default_unknown_upstream_404() {
        let (addr, _dir) = test_server().await;
        let (code, _) = http(addr, &put_route("ghost")).await;
        assert_eq!(code, 404);
    }

    #[tokio::test]
    async fn delete_route_default_clears_active_route() {
        let (addr, _dir) = test_server().await;
        http(addr, &post_upstream("us", "us.example", false)).await;
        http(addr, &put_route("us")).await;

        let (code, resp) = http(
            addr,
            "DELETE /v1/route/default HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(resp.contains("\"active_route\":null"), "got: {resp}");
    }

    #[tokio::test]
    async fn diff_endpoint_lists_shadows() {
        let (addr, _dir) = test_server().await;
        // shadow the cold 'default' with a runtime entry
        http(addr, &post_upstream("default", "hot.example", false)).await;
        let (code, body) = http(
            addr,
            "GET /v1/diff HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(body.contains("\"default\""), "got: {body}");
        assert!(body.contains("\"effective\":\"hot\""), "got: {body}");
    }

    #[tokio::test]
    async fn spawn_binds_and_serves_status() {
        // Exercises the real public entry point main.rs calls (bind + accept
        // loop), not just handle_conn in isolation.
        let dir = tempfile::tempdir().unwrap();
        let mut upstreams = BTreeMap::new();
        upstreams.insert(
            "default".to_string(),
            crate::config::Upstream {
                kind: crate::config::UpstreamKind::HttpConnect,
                host: "cold.example".into(),
                port: 823,
                auth: crate::config::UpstreamCreds {
                    username: "u".into(),
                    password: "p".into(),
                },
            },
        );
        let cold = Config {
            listen: Listen {
                addr: "127.0.0.1:0".parse().unwrap(),
                port_range: None,
                auth: ListenAuth::None,
            },
            upstreams,
            active_route: None,
            filter: crate::filter::FilterRules::default(),
            silo_floor_filter: crate::filter::FilterRules::default(),
        };
        let (store, _rx) = ConfigStore::new(cold, dir.path().join("runic.snapshot.json"));
        let store = Arc::new(Mutex::new(store));

        let addr = spawn(
            "127.0.0.1:0".parse().unwrap(),
            store,
            None,
            crate::stats::Stats::new(),
        )
        .await
        .unwrap();
        let (code, body) = http(
            addr,
            "GET /v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(body.contains("\"pool_size\":1"), "got: {body}");
    }
}
