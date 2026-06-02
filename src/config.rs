use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_UPSTREAM_NAME: &str = "default";

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: Listen,
    pub upstreams: BTreeMap<String, Upstream>,
    /// Name of the upstream the no-provider ("default") route resolves to,
    /// settable live via the admin API (`PUT /v1/route/default`). `None` falls
    /// back to the entry literally named `default`. This is a runtime pointer —
    /// switching the active provider by name, without re-sending its creds.
    pub active_route: Option<String>,
}

impl Config {
    /// The upstream a session falls back to when it doesn't pick a provider.
    /// `None` when there is no `default` entry — a valid state: runic tolerates
    /// an empty / default-less pool and is driven live via the admin API.
    /// Sessions with no matching route fail cleanly (see `routing::pick_upstream`).
    pub fn default_upstream(&self) -> Option<&Upstream> {
        self.upstreams.get(DEFAULT_UPSTREAM_NAME)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Listen {
    pub addr: SocketAddr,
    #[serde(default)]
    pub auth: ListenAuth,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenAuth {
    #[default]
    None,
}

/// Admin control-plane listener config. Boot-time only (the admin address is
/// read once from the cold YAML; changing it requires a restart). Loopback by
/// default — the admin API has no auth, so the bind address is the trust
/// boundary, same stance as the SOCKS5 surface.
#[derive(Debug, Clone, Deserialize)]
pub struct Admin {
    pub addr: SocketAddr,
}

impl Default for Admin {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([127, 0, 0, 1], 7778)),
        }
    }
}

/// Transport kind for an upstream. `HttpConnect` relays through a gateway (the
/// production path). `Direct` makes a plain TCP connect straight to the target
/// — NOT proxied, local IP exposed — gated behind `RUNIC_ALLOW_DIRECT=1`, for
/// dev/CI use only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamKind {
    #[default]
    HttpConnect,
    Direct,
}

/// A resolved upstream entry. Credentials are in clear (resolved from env or
/// inline at construction time) so they can be (de)serialized for the snapshot
/// cache and the admin API. `UpstreamCreds`' manual `Debug` keeps the password
/// out of logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// `#[serde(default)]` so snapshots written before this field existed
    /// deserialize as `HttpConnect`.
    #[serde(default)]
    pub kind: UpstreamKind,
    pub host: String,
    pub port: u16,
    pub auth: UpstreamCreds,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamCreds {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for UpstreamCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamCreds")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct RawFile {
    listen: Listen,
    #[serde(default)]
    admin: Admin,
    upstreams: BTreeMap<String, UpstreamSpec>,
}

/// Wire shape of one upstream entry, shared by the cold YAML loader and the
/// admin API POST body. `kind` defaults to `http_connect`; credentials accept
/// either env-var indirection (`username_env`/`password_env`, cold YAML style)
/// or inline `username`/`password` (admin API style — allows credential
/// rotation without a restart).
#[derive(Debug, Deserialize)]
pub struct UpstreamSpec {
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Optional so a `kind: direct` entry can omit them (no gateway, no creds).
    /// Required for `kind: http_connect` (validated in `resolve`).
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub auth: Option<CredsSpec>,
}

fn default_kind() -> String {
    "http_connect".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum CredsSpec {
    /// Env-var indirection (cold YAML). Resolved at parse time.
    Env {
        username_env: String,
        password_env: String,
    },
    /// Inline credentials (admin API / snapshot). Used verbatim.
    Inline { username: String, password: String },
}

impl UpstreamSpec {
    /// Validate the kind and resolve credentials into a runtime `Upstream`.
    /// `name` is used only for error context. This is the single chokepoint
    /// both the cold YAML loader and the admin hot-add path go through, so the
    /// `direct` opt-in guard below covers every way a `direct` upstream can be
    /// introduced.
    pub fn resolve(self, name: &str) -> Result<Upstream> {
        match self.kind.as_str() {
            "http_connect" => {
                let host = self.host.ok_or_else(|| {
                    anyhow!("upstreams.{name}.host is required for kind=http_connect")
                })?;
                let port = self.port.ok_or_else(|| {
                    anyhow!("upstreams.{name}.port is required for kind=http_connect")
                })?;
                let auth = self.auth.ok_or_else(|| {
                    anyhow!("upstreams.{name}.auth is required for kind=http_connect")
                })?;
                Ok(Upstream {
                    kind: UpstreamKind::HttpConnect,
                    host,
                    port,
                    auth: resolve_creds(auth, name)?,
                })
            }
            "direct" => {
                // Direct mode is NOT proxied: a plain TCP connect to the target,
                // with the local IP exposed. Fail-closed behind an explicit env
                // opt-in so it can never reach prod by a config copy-paste.
                if std::env::var("RUNIC_ALLOW_DIRECT").ok().as_deref() != Some("1") {
                    return Err(anyhow!(
                        "upstreams.{name} kind=direct requires RUNIC_ALLOW_DIRECT=1 — \
                         direct mode is NOT proxied (local IP exposed), dev/CI only"
                    ));
                }
                // host/port/auth are meaningless for direct; ignore if present.
                Ok(Upstream {
                    kind: UpstreamKind::Direct,
                    host: String::new(),
                    port: 0,
                    auth: UpstreamCreds {
                        username: String::new(),
                        password: String::new(),
                    },
                })
            }
            other => Err(anyhow!(
                "upstreams.{name}.kind = '{other}' not supported (use 'http_connect' or 'direct')"
            )),
        }
    }
}

/// Resolve a [`CredsSpec`] into clear [`UpstreamCreds`] (env-var indirection or
/// inline). `name` is used only for error context.
fn resolve_creds(spec: CredsSpec, name: &str) -> Result<UpstreamCreds> {
    match spec {
        CredsSpec::Inline { username, password } => Ok(UpstreamCreds { username, password }),
        CredsSpec::Env {
            username_env,
            password_env,
        } => {
            let username = std::env::var(&username_env).with_context(|| {
                format!("env var {username_env} (upstreams.{name}.auth.username_env) not set")
            })?;
            let password = std::env::var(&password_env).with_context(|| {
                format!("env var {password_env} (upstreams.{name}.auth.password_env) not set")
            })?;
            Ok(UpstreamCreds { username, password })
        }
    }
}

impl Config {
    /// Load the data-plane config (listen + upstreams). Kept for callers that
    /// don't need the admin address (tests, watcher reload fallback).
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::load_with_admin(path)?.0)
    }

    /// Load the full cold config: the data-plane `Config` plus the boot-time
    /// `Admin` listener address.
    pub fn load_with_admin(path: &Path) -> Result<(Self, Admin)> {
        let raw =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        let file: RawFile =
            serde_yaml::from_str(&raw).with_context(|| format!("parse YAML {}", path.display()))?;

        // An empty pool, or a pool without a `default` entry, is valid: runic
        // tolerates a bare config and is driven live via the admin API. Sessions
        // that find no matching route fail cleanly (see `routing::pick_upstream`),
        // rather than runic refusing to boot.
        let mut upstreams = BTreeMap::new();
        for (name, spec) in file.upstreams {
            let up = spec.resolve(&name)?;
            upstreams.insert(name, up);
        }

        Ok((
            Config {
                listen: file.listen,
                upstreams,
                // The active-route pointer is a runtime (admin-API) concept; the
                // cold YAML doesn't set it. None = fall back to the `default` entry.
                active_route: None,
            },
            file.admin,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn yaml_with(kind: &str, user_env: &str, pass_env: &str) -> String {
        format!(
            r#"listen:
  addr: "127.0.0.1:7777"
upstreams:
  default:
    kind: {kind}
    host: gw.example.com
    port: 823
    auth:
      username_env: {user_env}
      password_env: {pass_env}
"#
        )
    }

    fn write_tmp(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn loads_valid_yaml() {
        std::env::set_var("RUNIC_T_CFG_USER_OK", "alice");
        std::env::set_var("RUNIC_T_CFG_PASS_OK", "s3cret");
        let f = write_tmp(&yaml_with(
            "http_connect",
            "RUNIC_T_CFG_USER_OK",
            "RUNIC_T_CFG_PASS_OK",
        ));

        let cfg = Config::load(f.path()).unwrap();

        let up = cfg.default_upstream().unwrap();
        assert_eq!(up.kind, UpstreamKind::HttpConnect);
        assert_eq!(up.host, "gw.example.com");
        assert_eq!(up.port, 823);
        assert_eq!(up.auth.username, "alice");
        assert_eq!(up.auth.password, "s3cret");
        assert!(matches!(cfg.listen.auth, ListenAuth::None));
        assert_eq!(cfg.upstreams.len(), 1);
    }

    #[test]
    fn loads_pool_with_multiple_named_upstreams() {
        std::env::set_var("RUNIC_T_CFG_U_FR", "fr_user");
        std::env::set_var("RUNIC_T_CFG_P_FR", "fr_pass");
        std::env::set_var("RUNIC_T_CFG_U_US", "us_user");
        std::env::set_var("RUNIC_T_CFG_P_US", "us_pass");

        let yaml = r#"listen:
  addr: "127.0.0.1:7777"
upstreams:
  default:
    kind: http_connect
    host: gw-fr.example.com
    port: 823
    auth:
      username_env: RUNIC_T_CFG_U_FR
      password_env: RUNIC_T_CFG_P_FR
  us-residential:
    kind: http_connect
    host: gw-us.example.com
    port: 823
    auth:
      username_env: RUNIC_T_CFG_U_US
      password_env: RUNIC_T_CFG_P_US
"#;
        let f = write_tmp(yaml);
        let cfg = Config::load(f.path()).unwrap();

        assert_eq!(cfg.upstreams.len(), 2);
        assert_eq!(cfg.default_upstream().unwrap().host, "gw-fr.example.com");
        assert_eq!(cfg.upstreams["us-residential"].host, "gw-us.example.com");
        assert_eq!(cfg.upstreams["us-residential"].auth.username, "us_user");
    }

    #[test]
    fn loads_empty_upstreams_pool() {
        // An empty pool is valid now (silo / API-driven mode): runic boots and
        // is configured live via the admin API.
        let yaml = r#"listen:
  addr: "127.0.0.1:7777"
upstreams: {}
"#;
        let f = write_tmp(yaml);
        let cfg = Config::load(f.path()).unwrap();
        assert!(cfg.upstreams.is_empty());
        assert!(cfg.default_upstream().is_none());
    }

    #[test]
    fn loads_pool_without_default_entry() {
        std::env::set_var("RUNIC_T_CFG_U_NODEF", "u");
        std::env::set_var("RUNIC_T_CFG_P_NODEF", "p");
        let yaml = r#"listen:
  addr: "127.0.0.1:7777"
upstreams:
  primary:
    kind: http_connect
    host: gw.example.com
    port: 823
    auth:
      username_env: RUNIC_T_CFG_U_NODEF
      password_env: RUNIC_T_CFG_P_NODEF
"#;
        let f = write_tmp(yaml);
        let cfg = Config::load(f.path()).unwrap();
        // Loads fine; there's simply no fallback route until one is named.
        assert_eq!(cfg.upstreams.len(), 1);
        assert!(cfg.default_upstream().is_none());
        assert_eq!(cfg.upstreams["primary"].host, "gw.example.com");
    }

    #[test]
    fn direct_kind_gated_by_allow_env() {
        // One test (not two) so the process-global `RUNIC_ALLOW_DIRECT` mutation
        // is sequential — no parallel-test race. No other test touches this var.
        let yaml = r#"listen:
  addr: "127.0.0.1:7777"
upstreams:
  default:
    kind: direct
"#;
        // Without the opt-in → fail-closed.
        std::env::remove_var("RUNIC_ALLOW_DIRECT");
        let f = write_tmp(yaml);
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("RUNIC_ALLOW_DIRECT"), "got: {msg}");
        assert!(msg.contains("direct"), "got: {msg}");

        // With the opt-in → resolves as a Direct upstream.
        std::env::set_var("RUNIC_ALLOW_DIRECT", "1");
        let cfg = Config::load(f.path()).unwrap();
        std::env::remove_var("RUNIC_ALLOW_DIRECT");
        assert_eq!(cfg.default_upstream().unwrap().kind, UpstreamKind::Direct);
    }

    #[test]
    fn http_connect_missing_host_errors() {
        let yaml = r#"listen:
  addr: "127.0.0.1:7777"
upstreams:
  default:
    kind: http_connect
    auth:
      username: u
      password: p
"#;
        let f = write_tmp(yaml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("host"), "got: {err}");
    }

    #[test]
    fn rejects_malformed_yaml() {
        let f = write_tmp("this: is: not: valid: yaml: structure: at: all");
        let err = Config::load(f.path()).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("parse")
                || err.chain().any(|c| {
                    let s = c.to_string().to_lowercase();
                    s.contains("yaml") || s.contains("expected") || s.contains("mapping")
                }),
            "expected YAML parse error, got: {err:#}"
        );
    }

    #[test]
    fn rejects_unknown_upstream_kind() {
        std::env::set_var("RUNIC_T_CFG_USER_KIND", "u");
        std::env::set_var("RUNIC_T_CFG_PASS_KIND", "p");
        let f = write_tmp(&yaml_with(
            "socks5",
            "RUNIC_T_CFG_USER_KIND",
            "RUNIC_T_CFG_PASS_KIND",
        ));

        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("http_connect"), "got: {msg}");
        assert!(msg.contains("socks5"), "got: {msg}");
    }

    #[test]
    fn missing_env_var_errors_with_var_name() {
        std::env::remove_var("RUNIC_T_CFG_USER_MISSING");
        std::env::remove_var("RUNIC_T_CFG_PASS_MISSING");
        let f = write_tmp(&yaml_with(
            "http_connect",
            "RUNIC_T_CFG_USER_MISSING",
            "RUNIC_T_CFG_PASS_MISSING",
        ));

        let err = Config::load(f.path()).unwrap_err();
        assert!(
            err.to_string().contains("RUNIC_T_CFG_USER_MISSING"),
            "error should name the missing var, got: {err:#}"
        );
    }

    #[test]
    fn upstream_creds_debug_redacts_password() {
        let creds = UpstreamCreds {
            username: "alice".to_string(),
            password: "super-secret-password".to_string(),
        };
        let dbg = format!("{creds:?}");
        assert!(dbg.contains("alice"), "username should be visible: {dbg}");
        assert!(
            !dbg.contains("super-secret-password"),
            "password must be redacted: {dbg}"
        );
        assert!(
            dbg.contains("redacted"),
            "expected explicit redaction marker: {dbg}"
        );
    }
}
