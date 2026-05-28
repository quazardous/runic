//! V0.7 session-layer routing.
//!
//! The SOCKS5 username field carries the client's routing intent in the form
//! `key=value;key=value;...`. The routing layer parses that into a
//! [`RoutingSpec`], looks the `provider` up in the runtime upstream pool, and
//! falls back to the `default` entry for unknown providers or no-auth clients.
//!
//! Unknown keys are ignored silently — this keeps the format forward
//! compatible: new V0.7.1+ keys (`sessid`, etc.) won't break older runic
//! versions that don't understand them.

use crate::config::{Config, Upstream};

/// Decoded routing intent from the SOCKS5 username.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingSpec {
    /// Name of the upstream entry to use, looked up in `cfg.upstreams`.
    pub provider: Option<String>,
    /// Session identifier — reserved for V0.7.1 sticky-session routing,
    /// captured here but not yet acted upon.
    pub sessid: Option<String>,
}

/// Parse a SOCKS5 username into a [`RoutingSpec`]. Returns an empty spec for
/// `None`, empty strings, malformed segments, or unknown keys.
pub fn parse_routing_spec(socks5_user: Option<&str>) -> RoutingSpec {
    let mut spec = RoutingSpec::default();
    let Some(raw) = socks5_user else { return spec };
    if raw.is_empty() {
        return spec;
    }
    for segment in raw.split(';') {
        let mut parts = segment.splitn(2, '=');
        let Some(key) = parts.next() else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = parts.next().unwrap_or("").trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "provider" => spec.provider = Some(value.to_string()),
            "sessid" => spec.sessid = Some(value.to_string()),
            _ => { /* forward-compat: ignore unknown keys */ }
        }
    }
    spec
}

/// Resolve the upstream that should serve this session. Picks the named
/// `provider` from the pool when present, otherwise falls back to `default`.
/// `default` is always present in a valid `Config` (validated at load time).
pub fn pick_upstream<'a>(cfg: &'a Config, socks5_user: Option<&str>) -> &'a Upstream {
    let spec = parse_routing_spec(socks5_user);
    spec.provider
        .as_ref()
        .and_then(|name| cfg.upstreams.get(name))
        .unwrap_or_else(|| cfg.default_upstream())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Listen, ListenAuth, Upstream, UpstreamCreds, DEFAULT_UPSTREAM_NAME};
    use std::collections::BTreeMap;

    fn upstream_with_host(host: &str) -> Upstream {
        Upstream {
            host: host.to_string(),
            port: 823,
            auth: UpstreamCreds {
                username: "u".to_string(),
                password: "p".to_string(),
            },
        }
    }

    fn cfg_with(default_host: &str, others: &[(&str, &str)]) -> Config {
        let mut upstreams = BTreeMap::new();
        upstreams.insert(
            DEFAULT_UPSTREAM_NAME.to_string(),
            upstream_with_host(default_host),
        );
        for (name, host) in others {
            upstreams.insert((*name).to_string(), upstream_with_host(host));
        }
        Config {
            listen: Listen {
                addr: "127.0.0.1:0".parse().unwrap(),
                auth: ListenAuth::None,
            },
            upstreams,
        }
    }

    // parse_routing_spec ------------------------------------------------------

    #[test]
    fn parse_empty_or_none_yields_default_spec() {
        assert_eq!(parse_routing_spec(None), RoutingSpec::default());
        assert_eq!(parse_routing_spec(Some("")), RoutingSpec::default());
    }

    #[test]
    fn parse_extracts_provider() {
        let spec = parse_routing_spec(Some("provider=us-residential"));
        assert_eq!(spec.provider.as_deref(), Some("us-residential"));
        assert_eq!(spec.sessid, None);
    }

    #[test]
    fn parse_handles_multiple_keys() {
        let spec = parse_routing_spec(Some("provider=fr;sessid=xyz123"));
        assert_eq!(spec.provider.as_deref(), Some("fr"));
        assert_eq!(spec.sessid.as_deref(), Some("xyz123"));
    }

    #[test]
    fn parse_ignores_unknown_keys_for_forward_compat() {
        let spec = parse_routing_spec(Some("provider=us;futureKey=value;another=42"));
        assert_eq!(spec.provider.as_deref(), Some("us"));
        assert_eq!(spec.sessid, None);
    }

    #[test]
    fn parse_skips_malformed_segments() {
        // empty key (`=value`), empty value (`key=`), bare key (no `=`) all skipped.
        let spec = parse_routing_spec(Some("=orphan;provider=ok;loneKey;empty=;sessid=z"));
        assert_eq!(spec.provider.as_deref(), Some("ok"));
        assert_eq!(spec.sessid.as_deref(), Some("z"));
    }

    // pick_upstream -----------------------------------------------------------

    #[test]
    fn picks_named_provider_when_present() {
        let cfg = cfg_with("gw-default.example", &[("us-residential", "gw-us.example")]);
        let up = pick_upstream(&cfg, Some("provider=us-residential"));
        assert_eq!(up.host, "gw-us.example");
    }

    #[test]
    fn falls_back_to_default_when_provider_missing_from_pool() {
        let cfg = cfg_with("gw-default.example", &[]);
        let up = pick_upstream(&cfg, Some("provider=does-not-exist"));
        assert_eq!(up.host, "gw-default.example");
    }

    #[test]
    fn falls_back_to_default_when_user_empty() {
        let cfg = cfg_with("gw-default.example", &[]);
        assert_eq!(pick_upstream(&cfg, None).host, "gw-default.example");
        assert_eq!(pick_upstream(&cfg, Some("")).host, "gw-default.example");
    }

    #[test]
    fn falls_back_to_default_on_malformed_spec() {
        let cfg = cfg_with("gw-default.example", &[("us", "gw-us.example")]);
        // No `provider=` key at all → should land on default, not us.
        assert_eq!(pick_upstream(&cfg, Some("sessid=xyz")).host, "gw-default.example");
        assert_eq!(pick_upstream(&cfg, Some("=junk")).host, "gw-default.example");
    }
}
