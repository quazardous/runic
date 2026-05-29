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
}

impl Config {
    /// The upstream used by the current single-route code path. Future routing
    /// layers will replace this with a per-session pick over the full pool.
    pub fn default_upstream(&self) -> &Upstream {
        self.upstreams
            .get(DEFAULT_UPSTREAM_NAME)
            .expect("'default' upstream is required (validated in Config::load)")
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

/// A resolved upstream entry. Credentials are in clear (resolved from env or
/// inline at construction time) so they can be (de)serialized for the snapshot
/// cache and the admin API. `UpstreamCreds`' manual `Debug` keeps the password
/// out of logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
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
    pub host: String,
    pub port: u16,
    pub auth: CredsSpec,
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
    /// `name` is used only for error context.
    pub fn resolve(self, name: &str) -> Result<Upstream> {
        if self.kind != "http_connect" {
            return Err(anyhow!(
                "upstreams.{name}.kind = '{}' not supported (only 'http_connect')",
                self.kind
            ));
        }
        let auth = match self.auth {
            CredsSpec::Inline { username, password } => UpstreamCreds { username, password },
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
                UpstreamCreds { username, password }
            }
        };
        Ok(Upstream {
            host: self.host,
            port: self.port,
            auth,
        })
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
        let raw = fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let file: RawFile = serde_yaml::from_str(&raw)
            .with_context(|| format!("parse YAML {}", path.display()))?;

        if file.upstreams.is_empty() {
            return Err(anyhow!(
                "config has no upstreams; declare at least one (e.g. `upstreams.default`)"
            ));
        }
        if !file.upstreams.contains_key(DEFAULT_UPSTREAM_NAME) {
            let names: Vec<&str> = file.upstreams.keys().map(String::as_str).collect();
            return Err(anyhow!(
                "config has no upstream named '{DEFAULT_UPSTREAM_NAME}'; \
                 declared upstreams: {names:?}. \
                 The current release routes all traffic through 'default'."
            ));
        }

        let mut upstreams = BTreeMap::new();
        for (name, spec) in file.upstreams {
            let up = spec.resolve(&name)?;
            upstreams.insert(name, up);
        }

        Ok((
            Config {
                listen: file.listen,
                upstreams,
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
        let f = write_tmp(&yaml_with("http_connect", "RUNIC_T_CFG_USER_OK", "RUNIC_T_CFG_PASS_OK"));

        let cfg = Config::load(f.path()).unwrap();

        let up = cfg.default_upstream();
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
        assert_eq!(cfg.default_upstream().host, "gw-fr.example.com");
        assert_eq!(cfg.upstreams["us-residential"].host, "gw-us.example.com");
        assert_eq!(cfg.upstreams["us-residential"].auth.username, "us_user");
    }

    #[test]
    fn rejects_empty_upstreams_pool() {
        let yaml = r#"listen:
  addr: "127.0.0.1:7777"
upstreams: {}
"#;
        let f = write_tmp(yaml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("no upstreams"), "got: {err}");
    }

    #[test]
    fn rejects_pool_without_default_entry() {
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
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'default'"), "got: {msg}");
        assert!(msg.contains("primary"), "got: {msg}");
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
        let f = write_tmp(&yaml_with("socks5", "RUNIC_T_CFG_USER_KIND", "RUNIC_T_CFG_PASS_KIND"));

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
        assert!(dbg.contains("redacted"), "expected explicit redaction marker: {dbg}");
    }
}
