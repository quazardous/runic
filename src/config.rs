use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::filter::FilterRules;

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
    /// Instance-wide domain filter applied at CONNECT time (before dialing the
    /// upstream) to **non-silo** sessions. This is the *merged* filter: the cold
    /// YAML plus any admin-API runtime/permanent overrides. Empty = allow
    /// everything (the default).
    pub filter: FilterRules,
    /// The **static file** (cold-YAML) filter, kept separate from `filter` so it
    /// can serve as the floor a silo session composes on top of. Only the
    /// declarative file layer floors silos — the admin-API runtime/permanent
    /// layers deliberately never reach a silo (a token-less mutation must not
    /// pierce silo isolation). See [`crate::filter::decide_session`].
    pub silo_floor_filter: FilterRules,
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

/// SOCKS5 data-plane listener config. Loopback by default — the listener has
/// no auth (outside silo binding), so the bind address is the trust boundary.
#[derive(Debug, Clone, Deserialize)]
pub struct Listen {
    #[serde(default = "default_listen_addr")]
    pub addr: SocketAddr,
    #[serde(default)]
    pub auth: ListenAuth,
}

/// `7878` rather than the crowded `7777` neighbourhood, mirroring the admin
/// port's "quiet default" stance (see [`Admin`]).
fn default_listen_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 7878))
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            addr: default_listen_addr(),
            auth: ListenAuth::default(),
        }
    }
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
            addr: SocketAddr::from(([127, 0, 0, 1], 48484)),
        }
    }
}

/// Per-silo settings (cold YAML `silo:` section). **Opt-in**: when absent or
/// `enabled: false`, runic runs in plain mode (cleartext snapshot, unchanged).
/// When enabled, config is kept as encrypted per-variation snapshots (see
/// [`crate::silo`]).
#[derive(Debug, Clone, Deserialize)]
pub struct SiloConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Variation idle TTL, in days; past it the GC purges the variation.
    #[serde(default = "default_silo_ttl_days")]
    pub ttl_days: u64,
    /// How a client binds to its variation.
    #[serde(default)]
    pub auth: SiloAuth,
}

/// Client→variation binding mode for a silo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiloAuth {
    /// Token presented in the SOCKS5 password (RFC 1929). The **default** — works
    /// with auth-capable clients (curl, SDKs).
    #[default]
    Rfc1929,
    /// No SOCKS5 auth; the client binds to a per-variation loopback port instead
    /// (for clients like Chromium that don't speak SOCKS5 auth).
    None,
}

fn default_silo_ttl_days() -> u64 {
    7
}

/// Transport kind for an upstream. `HttpConnect` relays through a gateway (the
/// production path). `Direct` makes a plain TCP connect straight to the target
/// — NOT proxied, local IP exposed. Allowed by default (a `direct` upstream must
/// always be declared explicitly — it is never implicit); set
/// `RUNIC_ALLOW_DIRECT=0` to forbid it outright (prod hardening).
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

/// Every field defaults, so a config that sets nothing — or a file that is
/// nothing but comments — boots on built-in defaults (loopback listeners,
/// empty pool, no filter). The shipped package config relies on this: it is
/// fully commented out, documenting the defaults instead of restating them.
#[derive(Debug, Default, Deserialize)]
struct RawFile {
    #[serde(default)]
    listen: Listen,
    #[serde(default)]
    admin: Admin,
    /// Optional so a bare config (e.g. silo mode, configured via the API) parses.
    #[serde(default)]
    upstreams: BTreeMap<String, UpstreamSpec>,
    #[serde(default)]
    silo: Option<SiloConfig>,
    /// Instance-wide domain filter. Absent = allow everything.
    #[serde(default)]
    filter: FilterRules,
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
    /// `direct` opt-out guard below (`RUNIC_ALLOW_DIRECT=0`) covers every way a
    /// `direct` upstream can be introduced.
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
                // with the local IP exposed. It is allowed by default — the guard
                // against accidental direct is that an upstream is never implicit:
                // you must explicitly declare a `kind: direct` entry to use it.
                // For prod hardening, `RUNIC_ALLOW_DIRECT=0` forbids it outright.
                if std::env::var("RUNIC_ALLOW_DIRECT").ok().as_deref() == Some("0") {
                    return Err(anyhow!(
                        "upstreams.{name} kind=direct is forbidden by RUNIC_ALLOW_DIRECT=0 — \
                         direct mode is NOT proxied (local IP exposed)"
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

    /// Load the full cold config: the data-plane `Config`, the boot-time `Admin`
    /// listener address, and the optional `silo` settings.
    pub fn load_with_admin(path: &Path) -> Result<(Self, Admin, Option<SiloConfig>)> {
        let raw =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        // `Option<_>`: an empty or comment-only file parses as a null document,
        // which must mean "all defaults", not a parse error.
        let file: RawFile = serde_yaml::from_str::<Option<RawFile>>(&raw)
            .with_context(|| format!("parse YAML {}", path.display()))?
            .unwrap_or_default();

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
                // For a bare cold config both are the file filter; the store
                // overlays runtime/permanent onto `filter` only, keeping
                // `silo_floor_filter` pinned to the file layer.
                filter: file.filter.clone(),
                silo_floor_filter: file.filter,
            },
            file.admin,
            file.silo,
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
  addr: "127.0.0.1:7878"
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
    fn comment_only_yaml_boots_on_defaults() {
        // The shipped package config is fully commented out — it must load as
        // "all built-in defaults", the same as an absent key-by-key config.
        let f = write_tmp("# every key commented out\n# listen:\n#   addr: \"127.0.0.1:7878\"\n");
        let (cfg, admin, silo) = Config::load_with_admin(f.path()).expect("comment-only loads");
        assert_eq!(cfg.listen.addr, "127.0.0.1:7878".parse().unwrap());
        assert_eq!(admin.addr, "127.0.0.1:48484".parse().unwrap());
        assert!(cfg.upstreams.is_empty());
        assert!(silo.is_none());
        assert!(cfg.filter.rules.is_empty());
        assert!(cfg.silo_floor_filter.rules.is_empty());
    }

    #[test]
    fn listen_defaults_when_only_addr_missing() {
        // `listen:` present but `addr` omitted — the field-level default kicks in.
        let f = write_tmp("listen:\n  auth: none\nupstreams: {}\n");
        let cfg = Config::load(f.path()).expect("addr-less listen loads");
        assert_eq!(cfg.listen.addr, "127.0.0.1:7878".parse().unwrap());
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
  addr: "127.0.0.1:7878"
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
  addr: "127.0.0.1:7878"
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
  addr: "127.0.0.1:7878"
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
    fn direct_kind_allowed_by_default_forbidden_by_env_0() {
        // One test (not two) so the process-global `RUNIC_ALLOW_DIRECT` mutation
        // is sequential — no parallel-test race. No other test touches this var.
        let yaml = r#"listen:
  addr: "127.0.0.1:7878"
upstreams:
  default:
    kind: direct
"#;
        let f = write_tmp(yaml);

        // Default (env unset) → direct is allowed: an explicit `kind: direct`
        // entry is the intentional act; nothing implicit.
        std::env::remove_var("RUNIC_ALLOW_DIRECT");
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.default_upstream().unwrap().kind, UpstreamKind::Direct);

        // Explicit opt-out `=0` → forbidden (prod hardening).
        std::env::set_var("RUNIC_ALLOW_DIRECT", "0");
        let err = Config::load(f.path()).unwrap_err();
        std::env::remove_var("RUNIC_ALLOW_DIRECT");
        let msg = err.to_string();
        assert!(msg.contains("RUNIC_ALLOW_DIRECT=0"), "got: {msg}");
        assert!(msg.contains("direct"), "got: {msg}");
    }

    #[test]
    fn http_connect_missing_host_errors() {
        let yaml = r#"listen:
  addr: "127.0.0.1:7878"
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
    fn parses_silo_config() {
        let yaml = r#"listen:
  addr: "127.0.0.1:7878"
silo:
  enabled: true
  ttl_days: 14
  auth: none
"#;
        let f = write_tmp(yaml);
        let (_cfg, _admin, silo) = Config::load_with_admin(f.path()).unwrap();
        let silo = silo.expect("silo section present");
        assert!(silo.enabled);
        assert_eq!(silo.ttl_days, 14);
        assert_eq!(silo.auth, SiloAuth::None);
    }

    #[test]
    fn silo_absent_is_none_and_defaults_apply() {
        // Absent silo section → None.
        let f = write_tmp("listen:\n  addr: \"127.0.0.1:7878\"\nupstreams: {}\n");
        let (_c, _a, silo) = Config::load_with_admin(f.path()).unwrap();
        assert!(silo.is_none());

        // Enabled with no auth/ttl given → defaults: 7 days, rfc1929.
        let f2 = write_tmp("listen:\n  addr: \"127.0.0.1:7878\"\nsilo:\n  enabled: true\n");
        let (_c2, _a2, silo2) = Config::load_with_admin(f2.path()).unwrap();
        let s = silo2.unwrap();
        assert_eq!(s.ttl_days, 7);
        assert_eq!(s.auth, SiloAuth::Rfc1929);
    }

    #[test]
    fn parses_filter_section() {
        use crate::filter::{Action, Rule};
        let yaml = r#"listen:
  addr: "127.0.0.1:7878"
upstreams: {}
filter:
  default: allow
  rules:
    - deny: "*.doubleclick.net"
    - allow: "cdn.mysite.com"
"#;
        let f = write_tmp(yaml);
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.filter.default, Action::Allow);
        assert_eq!(cfg.filter.rules.len(), 2);
        assert_eq!(cfg.filter.rules[0], Rule::Deny("*.doubleclick.net".into()));
        assert_eq!(cfg.filter.rules[1], Rule::Allow("cdn.mysite.com".into()));
        // The file filter is also the silo floor (verbatim).
        assert_eq!(cfg.silo_floor_filter, cfg.filter);
    }

    #[test]
    fn filter_absent_is_noop() {
        let f = write_tmp("listen:\n  addr: \"127.0.0.1:7878\"\nupstreams: {}\n");
        let cfg = Config::load(f.path()).unwrap();
        assert!(cfg.filter.is_noop());
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
