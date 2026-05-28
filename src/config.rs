use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

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

#[derive(Debug, Clone)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
    pub auth: UpstreamCreds,
}

#[derive(Clone)]
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
    upstreams: BTreeMap<String, RawUpstream>,
}

#[derive(Debug, Deserialize)]
struct RawUpstream {
    kind: String,
    host: String,
    port: u16,
    auth: RawUpstreamAuth,
}

#[derive(Debug, Deserialize)]
struct RawUpstreamAuth {
    username_env: String,
    password_env: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
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
        for (name, raw) in file.upstreams {
            if raw.kind != "http_connect" {
                return Err(anyhow!(
                    "upstreams.{name}.kind = '{}' not supported (only 'http_connect')",
                    raw.kind
                ));
            }
            let username = std::env::var(&raw.auth.username_env).with_context(|| {
                format!(
                    "env var {} (upstreams.{name}.auth.username_env) not set",
                    raw.auth.username_env
                )
            })?;
            let password = std::env::var(&raw.auth.password_env).with_context(|| {
                format!(
                    "env var {} (upstreams.{name}.auth.password_env) not set",
                    raw.auth.password_env
                )
            })?;
            upstreams.insert(
                name,
                Upstream {
                    host: raw.host,
                    port: raw.port,
                    auth: UpstreamCreds { username, password },
                },
            );
        }

        Ok(Config {
            listen: file.listen,
            upstreams,
        })
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
