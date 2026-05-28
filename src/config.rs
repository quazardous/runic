use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: Listen,
    pub upstream: Upstream,
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
    upstream: RawUpstream,
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

        if file.upstream.kind != "http_connect" {
            return Err(anyhow!(
                "upstream.kind = '{}' not supported in V0 (only 'http_connect')",
                file.upstream.kind
            ));
        }

        let username = std::env::var(&file.upstream.auth.username_env).with_context(|| {
            format!(
                "env var {} (upstream.auth.username_env) not set",
                file.upstream.auth.username_env
            )
        })?;
        let password = std::env::var(&file.upstream.auth.password_env).with_context(|| {
            format!(
                "env var {} (upstream.auth.password_env) not set",
                file.upstream.auth.password_env
            )
        })?;

        Ok(Config {
            listen: file.listen,
            upstream: Upstream {
                host: file.upstream.host,
                port: file.upstream.port,
                auth: UpstreamCreds { username, password },
            },
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
upstream:
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

        assert_eq!(cfg.upstream.host, "gw.example.com");
        assert_eq!(cfg.upstream.port, 823);
        assert_eq!(cfg.upstream.auth.username, "alice");
        assert_eq!(cfg.upstream.auth.password, "s3cret");
        assert!(matches!(cfg.listen.auth, ListenAuth::None));
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

