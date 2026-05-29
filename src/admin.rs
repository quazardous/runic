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

use crate::config::UpstreamSpec;
use crate::store::ConfigStore;

const REQUEST_HEAD_LIMIT: usize = 16 * 1024;
const REQUEST_BODY_LIMIT: usize = 256 * 1024;

/// Bind the admin listener and spawn its accept loop. Returns the bound address
/// once the bind succeeds so the caller can fail fast on a port clash (and tests
/// can target an OS-assigned `:0` port).
pub async fn spawn(addr: SocketAddr, store: Arc<Mutex<ConfigStore>>) -> Result<SocketAddr> {
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
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(sock, store, started).await {
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
}

async fn handle_conn(
    mut sock: TcpStream,
    store: Arc<Mutex<ConfigStore>>,
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

    let resp = route(&req, &store, started).await;
    sock.write_all(&resp).await.context("write admin response")?;
    sock.flush().await.ok();
    Ok(())
}

async fn route(req: &Request, store: &Arc<Mutex<ConfigStore>>, started: Instant) -> Vec<u8> {
    let permanent = query_flag(&req.query, "permanent");

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/status") => {
            let store = store.lock().await;
            json_response(
                200,
                "OK",
                &json!({
                    "status": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                    "uptime_secs": started.elapsed().as_secs(),
                    "pool_size": store.pool_size(),
                }),
            )
        }
        ("GET", "/v1/config") => {
            let cfg = store.lock().await.merged();
            json_response(
                200,
                "OK",
                &json!({
                    "listen": { "addr": cfg.listen.addr.to_string() },
                    "upstreams": cfg.upstreams,
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
        ("POST", path) if path.starts_with("/v1/upstreams/") => {
            let name = match upstream_name(path) {
                Some(n) => n,
                None => return json_response(400, "Bad Request", &json!({ "error": "missing upstream name" })),
            };
            let spec: UpstreamSpec = match serde_json::from_slice(&req.body) {
                Ok(s) => s,
                Err(e) => {
                    return json_response(400, "Bad Request", &json!({ "error": format!("invalid upstream body: {e}") }))
                }
            };
            let up = match spec.resolve(&name) {
                Ok(up) => up,
                Err(e) => return json_response(400, "Bad Request", &json!({ "error": e.to_string() })),
            };
            let mut store = store.lock().await;
            if permanent {
                if let Err(e) = store.apply_permanent(name.clone(), up) {
                    return persist_error(e);
                }
            } else {
                store.apply_runtime(name.clone(), up);
            }
            let source = store.diagnose().get(&name).copied();
            json_response(200, "OK", &json!({ "name": name, "permanent": permanent, "source": source }))
        }
        ("DELETE", path) if path.starts_with("/v1/upstreams/") => {
            let name = match upstream_name(path) {
                Some(n) => n,
                None => return json_response(400, "Bad Request", &json!({ "error": "missing upstream name" })),
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
                return json_response(404, "Not Found", &json!({ "error": format!("no runtime/snapshot entry '{name}'") }));
            }
            let fallback = store.diagnose().get(&name).copied();
            json_response(200, "OK", &json!({ "name": name, "removed": true, "permanent": permanent, "now": fallback }))
        }
        _ => json_response(404, "Not Found", &json!({ "error": "no such route" })),
    }
}

fn persist_error(e: anyhow::Error) -> Vec<u8> {
    warn!(error = %e, "admin snapshot persistence failed");
    json_response(500, "Internal Server Error", &json!({ "error": e.to_string() }))
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

    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| anyhow!("request head not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or_else(|| anyhow!("empty request"))?;
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or_else(|| anyhow!("missing method"))?.to_string();
    let target = parts.next().ok_or_else(|| anyhow!("missing target"))?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
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
        let n = sock.read(&mut chunk).await.context("read admin request body")?;
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
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
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
                host: "cold.example".into(),
                port: 823,
                auth: crate::config::UpstreamCreds { username: "u".into(), password: "p".into() },
            },
        );
        let cold = Config {
            listen: Listen {
                addr: "127.0.0.1:0".parse().unwrap(),
                auth: ListenAuth::None,
            },
            upstreams,
        };
        let (store, _rx) = ConfigStore::new(cold, dir.path().join("runic.snapshot.json"));
        let store = Arc::new(Mutex::new(store));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let started = Instant::now();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let store = store.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, store, started).await;
                });
            }
        });
        (addr, dir)
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
        let (code, body) = http(addr, "GET /v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
        assert_eq!(code, 200);
        assert!(body.contains("\"pool_size\":1"), "got: {body}");
        assert!(body.contains("\"status\":\"ok\""), "got: {body}");
    }

    #[tokio::test]
    async fn post_upstream_runtime_only_then_diagnose_hot() {
        let (addr, _dir) = test_server().await;
        let (code, _) = http(addr, &post_upstream("us", "us.example", false)).await;
        assert_eq!(code, 200);

        let (_, body) = http(addr, "GET /v1/diagnose HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
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
        let (code, body) = http(addr, "DELETE /v1/upstreams/us HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
        assert_eq!(code, 200);
        assert!(body.contains("\"now\":\"snapshot\""), "got: {body}");
    }

    #[tokio::test]
    async fn delete_unknown_upstream_404() {
        let (addr, _dir) = test_server().await;
        let (code, _) = http(addr, "DELETE /v1/upstreams/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
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
        let (code, _) = http(addr, "GET /v1/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
        assert_eq!(code, 404);
    }

    #[tokio::test]
    async fn diff_endpoint_lists_shadows() {
        let (addr, _dir) = test_server().await;
        // shadow the cold 'default' with a runtime entry
        http(addr, &post_upstream("default", "hot.example", false)).await;
        let (code, body) = http(addr, "GET /v1/diff HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
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
                auth: ListenAuth::None,
            },
            upstreams,
        };
        let (store, _rx) = ConfigStore::new(cold, dir.path().join("runic.snapshot.json"));
        let store = Arc::new(Mutex::new(store));

        let addr = spawn("127.0.0.1:0".parse().unwrap(), store).await.unwrap();
        let (code, body) = http(
            addr,
            "GET /v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(code, 200);
        assert!(body.contains("\"pool_size\":1"), "got: {body}");
    }
}
