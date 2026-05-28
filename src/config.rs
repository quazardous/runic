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
