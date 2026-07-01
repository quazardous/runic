//! Domain filter — allow/deny rules evaluated at CONNECT time against the target
//! hostname.
//!
//! Host-level only: runic is a SOCKS5 → HTTP `CONNECT` tunnel, so it sees the
//! target `host:port` but never the encrypted payload. This filter therefore
//! blocks (or admits) *whole hosts* — image CDNs, ad/tracker domains, fonts —
//! without any TLS interception. It cannot tell an image request from an HTML
//! one on the *same* host; that needs client-side resource-type blocking or a
//! MITM proxy, both deliberately out of scope (see `docs/install/filtering.md`).
//!
//! Model: an **ordered rule list, first-match-wins**, with a `default` action —
//! the same shape as firewalld's rich rules and iptables' ordered chains. One
//! engine expresses both a blocklist (`default: allow` + `deny` rules) and a
//! strict allowlist (`default: deny` + `allow` rules).
//!
//! Prior art: the ordered first-match model follows firewalld rich rules /
//! iptables chains; blocking by hostname follows the spirit of adblock hostlists
//! (runic ships no bundled list — you declare your own rules).

use serde::{Deserialize, Serialize};

/// The verdict for a target: let the CONNECT through, or refuse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Allow,
    Deny,
}

/// One ordered rule: an action bound to a host pattern. The wire shape is a
/// single-key map — `{deny: "*.doubleclick.net"}` / `{allow: "cdn.site.com"}` —
/// so a YAML/JSON rule list reads as a firewalld-style ruleset. (An externally
/// tagged enum would serialize as a YAML `!deny` tag, which is why the rule goes
/// through the [`RuleWire`] proxy instead.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RuleWire", into = "RuleWire")]
pub enum Rule {
    Allow(String),
    Deny(String),
}

/// Wire form of a [`Rule`]: exactly one of `allow` / `deny` carries the host
/// pattern. Kept private — [`Rule`] is the public type.
#[derive(Debug, Serialize, Deserialize)]
struct RuleWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deny: Option<String>,
}

impl TryFrom<RuleWire> for Rule {
    type Error = String;

    fn try_from(w: RuleWire) -> Result<Self, Self::Error> {
        match (w.allow, w.deny) {
            (Some(p), None) => Ok(Rule::Allow(p)),
            (None, Some(p)) => Ok(Rule::Deny(p)),
            (Some(_), Some(_)) => {
                Err("a filter rule sets both `allow` and `deny`; use exactly one".into())
            }
            (None, None) => Err("a filter rule sets neither `allow` nor `deny`".into()),
        }
    }
}

impl From<Rule> for RuleWire {
    fn from(r: Rule) -> Self {
        match r {
            Rule::Allow(p) => RuleWire {
                allow: Some(p),
                deny: None,
            },
            Rule::Deny(p) => RuleWire {
                allow: None,
                deny: Some(p),
            },
        }
    }
}

impl Rule {
    fn action(&self) -> Action {
        match self {
            Rule::Allow(_) => Action::Allow,
            Rule::Deny(_) => Action::Deny,
        }
    }

    fn pattern(&self) -> &str {
        match self {
            Rule::Allow(p) | Rule::Deny(p) => p,
        }
    }
}

/// A filter ruleset. Applies at two levels — the global instance config and, in
/// silo mode, each variation's own config — composed by [`decide_session`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FilterRules {
    /// Verdict when no rule matches. `allow` (the default) makes the ruleset a
    /// blocklist; `deny` makes it a strict allowlist.
    #[serde(default)]
    pub default: Action,
    /// Ordered rules; the first whose pattern matches the target decides.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// **Global-only** knob: when true, a global `deny` is a hard floor even
    /// inside a silo — a silo may tighten (deny more) but never re-allow a
    /// globally-denied host. Default `false` = silo-sovereign. Ignored on a
    /// per-silo ruleset (a silo cannot impose a floor on itself).
    #[serde(default)]
    pub enforce_in_silo: bool,
}

impl FilterRules {
    /// First-match-wins verdict for `host:port`, falling back to `default`.
    pub fn decide(&self, host: &str, port: u16) -> Action {
        for rule in &self.rules {
            if host_matches(rule.pattern(), host, port) {
                return rule.action();
            }
        }
        self.default
    }

    /// True when the ruleset admits everything (no rules, `default: allow`) — an
    /// unconfigured filter. Lets [`decide_session`] tell whether a silo defines
    /// a filter of its own.
    pub fn is_noop(&self) -> bool {
        self.rules.is_empty() && self.default == Action::Allow
    }
}

/// Compose the global and (optional) per-silo filters for one session.
///
/// - **Non-silo session** → the global filter decides.
/// - **Silo session** (a variation resolved for this session):
///   1. if the global filter has `enforce_in_silo` set **and** it denies the
///      host, the CONNECT is denied — the operator's hard floor is inviolable;
///   2. otherwise the silo is **sovereign**: a variation that defines its own
///      filter governs its sessions entirely; one that defines none inherits the
///      global filter as a baseline (so an operator's rules still protect silo
///      clients that set nothing, rather than silently allowing everything).
pub fn decide_session(
    global: &FilterRules,
    silo: Option<&FilterRules>,
    host: &str,
    port: u16,
) -> Action {
    let Some(silo) = silo else {
        return global.decide(host, port);
    };
    // (1) Operator hard floor (opt-in): a global deny is inviolable in-silo.
    if global.enforce_in_silo && global.decide(host, port) == Action::Deny {
        return Action::Deny;
    }
    // (2) Silo-sovereign, with the global filter as a baseline for silos that
    //     declare no filter of their own.
    if silo.is_noop() {
        global.decide(host, port)
    } else {
        silo.decide(host, port)
    }
}

/// Does `pattern` match the target `host:port`?
///
/// `pattern` is `host` or `host:port`. The host part is either an exact name
/// (`example.com`, apex only) or a subdomain wildcard (`*.example.com`, any
/// subdomain but **not** the apex). Matching is ASCII-case-insensitive. A
/// `:port` suffix restricts the rule to that port; without it the rule matches
/// on any port.
///
/// IPv4 literals match exactly (`1.2.3.4`). Bracketless IPv6 literals are not
/// supported as patterns (the `:` would be read as a port separator) — filter
/// IPv6 targets by an enclosing hostname instead.
fn host_matches(pattern: &str, host: &str, port: u16) -> bool {
    let (pat_host, pat_port) = match pattern.rsplit_once(':') {
        // A trailing `:<digits>` is a port constraint; anything else (e.g. an
        // IPv6 literal) is treated as a bare host pattern.
        Some((h, p)) => match p.parse::<u16>() {
            Ok(pp) => (h, Some(pp)),
            Err(_) => (pattern, None),
        },
        None => (pattern, None),
    };
    if let Some(pp) = pat_port {
        if pp != port {
            return false;
        }
    }
    host_pattern_matches(pat_host, host)
}

fn host_pattern_matches(pat_host: &str, host: &str) -> bool {
    if let Some(suffix) = pat_host.strip_prefix("*.") {
        // `*.example.com` matches any subdomain but not the apex: the target
        // must end with `.example.com` and have something before that dot.
        let needle = format!(".{suffix}");
        host.len() > needle.len() && ends_with_ci(host, &needle)
    } else {
        eq_ci(pat_host, host)
    }
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.as_bytes().eq_ignore_ascii_case(b.as_bytes())
}

fn ends_with_ci(haystack: &str, suffix: &str) -> bool {
    haystack.len() >= suffix.len()
        && haystack.as_bytes()[haystack.len() - suffix.len()..]
            .eq_ignore_ascii_case(suffix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(p: &str) -> Rule {
        Rule::Deny(p.to_string())
    }
    fn allow(p: &str) -> Rule {
        Rule::Allow(p.to_string())
    }

    // --- host matching -------------------------------------------------------

    #[test]
    fn exact_matches_apex_only() {
        assert!(host_matches("example.com", "example.com", 443));
        assert!(!host_matches("example.com", "sub.example.com", 443));
        assert!(!host_matches("example.com", "notexample.com", 443));
    }

    #[test]
    fn wildcard_matches_subdomains_not_apex() {
        assert!(host_matches("*.example.com", "sub.example.com", 443));
        assert!(host_matches("*.example.com", "a.b.example.com", 443));
        assert!(!host_matches("*.example.com", "example.com", 443));
        // must not match a suffix that isn't on a dot boundary
        assert!(!host_matches("*.example.com", "notexample.com", 443));
        // empty subdomain label doesn't match
        assert!(!host_matches("*.example.com", ".example.com", 443));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(host_matches("Example.COM", "example.com", 443));
        assert!(host_matches("*.Example.com", "SUB.example.COM", 443));
    }

    #[test]
    fn port_constraint_is_honoured() {
        assert!(host_matches("example.com:443", "example.com", 443));
        assert!(!host_matches("example.com:443", "example.com", 80));
        // no port in the pattern → any port
        assert!(host_matches("example.com", "example.com", 80));
        assert!(host_matches("*.cdn.net:443", "img.cdn.net", 443));
        assert!(!host_matches("*.cdn.net:443", "img.cdn.net", 80));
    }

    #[test]
    fn ipv4_literal_matches_exactly() {
        assert!(host_matches("203.0.113.4", "203.0.113.4", 443));
        assert!(!host_matches("203.0.113.4", "203.0.113.5", 443));
    }

    // --- decide (single ruleset) ---------------------------------------------

    #[test]
    fn blocklist_denies_listed_allows_rest() {
        let f = FilterRules {
            default: Action::Allow,
            rules: vec![deny("*.doubleclick.net"), deny("google-analytics.com")],
            enforce_in_silo: false,
        };
        assert_eq!(f.decide("ads.doubleclick.net", 443), Action::Deny);
        assert_eq!(f.decide("google-analytics.com", 443), Action::Deny);
        assert_eq!(f.decide("example.com", 443), Action::Allow);
    }

    #[test]
    fn allowlist_mode_denies_unlisted() {
        let f = FilterRules {
            default: Action::Deny,
            rules: vec![allow("api.target.com"), allow("*.target.com")],
            enforce_in_silo: false,
        };
        assert_eq!(f.decide("api.target.com", 443), Action::Allow);
        assert_eq!(f.decide("cdn.target.com", 443), Action::Allow);
        assert_eq!(f.decide("evil.example", 443), Action::Deny);
    }

    #[test]
    fn first_match_wins_allow_exception_before_broad_deny() {
        let f = FilterRules {
            default: Action::Allow,
            rules: vec![allow("cdn.mysite.com"), deny("*.mysite.com")],
            enforce_in_silo: false,
        };
        // the allow is listed first, so it wins for that host
        assert_eq!(f.decide("cdn.mysite.com", 443), Action::Allow);
        // any other subdomain still hits the broad deny
        assert_eq!(f.decide("track.mysite.com", 443), Action::Deny);
    }

    #[test]
    fn empty_ruleset_is_noop_allow_all() {
        let f = FilterRules::default();
        assert!(f.is_noop());
        assert_eq!(f.decide("anything.example", 443), Action::Allow);
    }

    #[test]
    fn deny_default_without_rules_is_not_noop() {
        let f = FilterRules {
            default: Action::Deny,
            ..Default::default()
        };
        assert!(!f.is_noop());
    }

    // --- decide_session (global ∘ silo composition) --------------------------

    #[test]
    fn non_silo_uses_global() {
        let global = FilterRules {
            default: Action::Allow,
            rules: vec![deny("bad.example")],
            enforce_in_silo: false,
        };
        assert_eq!(
            decide_session(&global, None, "bad.example", 443),
            Action::Deny
        );
        assert_eq!(
            decide_session(&global, None, "ok.example", 443),
            Action::Allow
        );
    }

    #[test]
    fn silo_sovereign_ignores_global_when_it_has_its_own() {
        let global = FilterRules {
            default: Action::Allow,
            rules: vec![deny("blocked-by-op.example")],
            enforce_in_silo: false, // not a hard floor
        };
        let silo = FilterRules {
            default: Action::Allow,
            rules: vec![deny("blocked-by-client.example")],
            enforce_in_silo: false,
        };
        // global's deny does NOT apply to a sovereign silo session
        assert_eq!(
            decide_session(&global, Some(&silo), "blocked-by-op.example", 443),
            Action::Allow
        );
        // the silo's own deny does
        assert_eq!(
            decide_session(&global, Some(&silo), "blocked-by-client.example", 443),
            Action::Deny
        );
    }

    #[test]
    fn silo_without_own_filter_inherits_global_baseline() {
        let global = FilterRules {
            default: Action::Allow,
            rules: vec![deny("bad.example")],
            enforce_in_silo: false,
        };
        let empty = FilterRules::default();
        assert_eq!(
            decide_session(&global, Some(&empty), "bad.example", 443),
            Action::Deny
        );
    }

    #[test]
    fn enforce_in_silo_makes_global_deny_a_hard_floor() {
        let global = FilterRules {
            default: Action::Allow,
            rules: vec![deny("blocked-by-op.example")],
            enforce_in_silo: true, // hard floor
        };
        // silo tries to allow everything, but the global deny is inviolable
        let silo = FilterRules {
            default: Action::Allow,
            rules: vec![allow("blocked-by-op.example")],
            enforce_in_silo: false,
        };
        assert_eq!(
            decide_session(&global, Some(&silo), "blocked-by-op.example", 443),
            Action::Deny
        );
        // hosts the global doesn't block still follow the silo
        assert_eq!(
            decide_session(&global, Some(&silo), "elsewhere.example", 443),
            Action::Allow
        );
    }

    // --- serde wire shape ----------------------------------------------------

    #[test]
    fn rule_deserializes_from_single_key_map() {
        let y = r#"
default: allow
rules:
  - deny: "*.doubleclick.net"
  - allow: "cdn.mysite.com"
enforce_in_silo: true
"#;
        let f: FilterRules = serde_yaml::from_str(y).unwrap();
        assert_eq!(f.default, Action::Allow);
        assert!(f.enforce_in_silo);
        assert_eq!(f.rules.len(), 2);
        assert_eq!(f.rules[0], deny("*.doubleclick.net"));
        assert_eq!(f.rules[1], allow("cdn.mysite.com"));
    }

    #[test]
    fn absent_fields_default() {
        let f: FilterRules = serde_yaml::from_str("{}").unwrap();
        assert!(f.is_noop());
        assert!(!f.enforce_in_silo);
    }
}
