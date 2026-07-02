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

/// Decide allow/deny for one session, composing the instance and (optional)
/// per-silo filters.
///
/// - **Non-silo session** → the `instance` filter decides (its rules, then its
///   default). This is the merged instance filter (file plus any admin-API
///   runtime/permanent overrides).
/// - **Silo session** → the silo's own rules compose **on top of** the static
///   file `floor` (the cold-YAML global), which is the *only* thing that floors
///   a silo. The admin-API runtime/permanent layers deliberately never reach a
///   silo — a filter mutation without the client's token must not pierce silo
///   isolation. Evaluation:
///   1. the silo's own rules, first-match — they add to / override the floor;
///   2. a silo that declares `default: deny` is a **closed allowlist** — anything
///      its own rules didn't allow is denied, and the floor cannot loosen it;
///   3. otherwise (the additive case) fall through to the file `floor` (its
///      rules, then its default). A silo that sets no filter of its own is
///      therefore governed entirely by the file floor.
pub fn decide_session(
    instance: &FilterRules,
    silo: Option<&FilterRules>,
    floor: &FilterRules,
    host: &str,
    port: u16,
) -> Action {
    let Some(silo) = silo else {
        return instance.decide(host, port);
    };
    // (1) The silo's own rules win where they match — additive on top of the floor.
    for rule in &silo.rules {
        if host_matches(rule.pattern(), host, port) {
            return rule.action();
        }
    }
    // (2) A silo that opted into allowlist mode is closed; the floor can't reopen it.
    if silo.default == Action::Deny {
        return Action::Deny;
    }
    // (3) Additive silo → the static file floor governs the rest.
    floor.decide(host, port)
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

    fn rules(default: Action, rules: Vec<Rule>) -> FilterRules {
        FilterRules { default, rules }
    }

    #[test]
    fn blocklist_denies_listed_allows_rest() {
        let f = rules(
            Action::Allow,
            vec![deny("*.doubleclick.net"), deny("google-analytics.com")],
        );
        assert_eq!(f.decide("ads.doubleclick.net", 443), Action::Deny);
        assert_eq!(f.decide("google-analytics.com", 443), Action::Deny);
        assert_eq!(f.decide("example.com", 443), Action::Allow);
    }

    #[test]
    fn allowlist_mode_denies_unlisted() {
        let f = rules(
            Action::Deny,
            vec![allow("api.target.com"), allow("*.target.com")],
        );
        assert_eq!(f.decide("api.target.com", 443), Action::Allow);
        assert_eq!(f.decide("cdn.target.com", 443), Action::Allow);
        assert_eq!(f.decide("evil.example", 443), Action::Deny);
    }

    #[test]
    fn first_match_wins_allow_exception_before_broad_deny() {
        let f = rules(
            Action::Allow,
            vec![allow("cdn.mysite.com"), deny("*.mysite.com")],
        );
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

    // --- decide_session (instance / silo ∘ file-floor composition) -----------

    #[test]
    fn non_silo_uses_instance_filter() {
        let instance = rules(Action::Allow, vec![deny("bad.example")]);
        let floor = FilterRules::default();
        assert_eq!(
            decide_session(&instance, None, &floor, "bad.example", 443),
            Action::Deny
        );
        assert_eq!(
            decide_session(&instance, None, &floor, "ok.example", 443),
            Action::Allow
        );
    }

    #[test]
    fn silo_rules_compose_on_top_of_file_floor() {
        // File floor blocks a shared tracker for everyone.
        let floor = rules(Action::Allow, vec![deny("tracker.shared")]);
        // The silo adds its own module-specific deny.
        let silo = rules(Action::Allow, vec![deny("module.specific")]);
        // instance filter is irrelevant to a silo session (never consulted).
        let instance = rules(Action::Allow, vec![deny("via-noBearer-api.example")]);

        // the silo's own deny applies
        assert_eq!(
            decide_session(&instance, Some(&silo), &floor, "module.specific", 443),
            Action::Deny
        );
        // the file floor still applies where the silo is silent (compose, not replace)
        assert_eq!(
            decide_session(&instance, Some(&silo), &floor, "tracker.shared", 443),
            Action::Deny
        );
        // a host neither blocks → allowed
        assert_eq!(
            decide_session(&instance, Some(&silo), &floor, "fine.example", 443),
            Action::Allow
        );
        // the API-mutable instance layer never reaches a silo session
        assert_eq!(
            decide_session(
                &instance,
                Some(&silo),
                &floor,
                "via-noBearer-api.example",
                443
            ),
            Action::Allow
        );
    }

    #[test]
    fn silo_with_no_filter_is_governed_by_the_file_floor() {
        let floor = rules(Action::Allow, vec![deny("bad.example")]);
        let empty = FilterRules::default();
        let instance = FilterRules::default();
        assert_eq!(
            decide_session(&instance, Some(&empty), &floor, "bad.example", 443),
            Action::Deny
        );
        assert_eq!(
            decide_session(&instance, Some(&empty), &floor, "ok.example", 443),
            Action::Allow
        );
    }

    #[test]
    fn silo_allow_overrides_a_floor_deny() {
        // Overridable floor (the default): a silo may re-allow what the floor denies.
        let floor = rules(Action::Allow, vec![deny("*.cdn.example")]);
        let silo = rules(Action::Allow, vec![allow("img.cdn.example")]);
        let instance = FilterRules::default();
        // the silo's allow wins (evaluated first)
        assert_eq!(
            decide_session(&instance, Some(&silo), &floor, "img.cdn.example", 443),
            Action::Allow
        );
        // a sibling the silo didn't re-allow still hits the floor deny
        assert_eq!(
            decide_session(&instance, Some(&silo), &floor, "js.cdn.example", 443),
            Action::Deny
        );
    }

    #[test]
    fn silo_default_deny_is_a_closed_allowlist_floor_cannot_reopen() {
        // A silo that declares `default: deny` is a strict allowlist: anything
        // its own rules didn't allow is denied, regardless of the floor.
        let floor = rules(Action::Allow, vec![allow("floor-allows.example")]);
        let silo = rules(Action::Deny, vec![allow("api.target")]);
        let instance = FilterRules::default();
        assert_eq!(
            decide_session(&instance, Some(&silo), &floor, "api.target", 443),
            Action::Allow
        );
        // floor's allow does NOT leak into a closed-allowlist silo
        assert_eq!(
            decide_session(&instance, Some(&silo), &floor, "floor-allows.example", 443),
            Action::Deny
        );
        assert_eq!(
            decide_session(&instance, Some(&silo), &floor, "anything.else", 443),
            Action::Deny
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
"#;
        let f: FilterRules = serde_yaml::from_str(y).unwrap();
        assert_eq!(f.default, Action::Allow);
        assert_eq!(f.rules.len(), 2);
        assert_eq!(f.rules[0], deny("*.doubleclick.net"));
        assert_eq!(f.rules[1], allow("cdn.mysite.com"));
    }

    #[test]
    fn absent_fields_default() {
        let f: FilterRules = serde_yaml::from_str("{}").unwrap();
        assert!(f.is_noop());
    }

    #[test]
    fn legacy_enforce_in_silo_key_is_ignored_not_rejected() {
        // Old configs/snapshots may still carry the retired field; it must load,
        // not fail (FilterRules doesn't deny unknown fields).
        let f: FilterRules = serde_yaml::from_str("enforce_in_silo: true\n").unwrap();
        assert!(f.is_noop());
    }
}
