//! Live runtime counters for the status surface (`/v1/status` + the HTML page).
//!
//! Cheap in-RAM session accounting, updated only at session **start/end** (never
//! in the copy loop): how many SOCKS5 sessions are active right now (total, and
//! how many leak through a `direct` upstream — the icon's amber signal), a
//! cumulative request count, and per-variation active/cumulative counters.
//!
//! Stats live independently of the variation cache, so a variation's cumulative
//! request count survives idle eviction (warm→cold→warm) instead of resetting.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crate::config::UpstreamKind;
use crate::filter::Layer;

#[derive(Default)]
struct Inner {
    active_total: u64,
    /// Active sessions whose chosen upstream is `kind: direct` (local IP exposed).
    active_direct: u64,
    requests_total: u64,
    /// Cumulative CONNECTs refused by the domain filter (never begins a session).
    filtered_total: u64,
    /// Cumulative CONNECTs a `log_only` ruleset *would* have denied — they went
    /// through (a session began), only this counter records the dry-run verdict.
    would_filtered_total: u64,
    per_variation: HashMap<String, VarCounters>,
    /// Per-rule hit counters for the three filter layers. Debug-grade RAM-only
    /// state: keyed by the ruleset's content fingerprint, so replacing the
    /// rules (admin PUT/DELETE, file reload) resets the counters on their own.
    hits_instance: LayerHits,
    hits_floor: LayerHits,
    /// Silo layers, one per variation id. Ephemeral by design — nothing enters
    /// the encrypted blob; counters live as long as the process.
    hits_silo: HashMap<String, LayerHits>,
    /// The SOCKS5 listener's *actually bound* address, published on every
    /// (re)bind. Differs from the configured `listen.addr` in auto-port mode
    /// (`addr: "…:0"`), where the OS picks the port and the status surface is
    /// how a client discovers it. `None` until the first bind.
    bound_addr: Option<SocketAddr>,
}

#[derive(Default, Clone, Copy)]
struct VarCounters {
    active: u64,
    requests: u64,
    filtered: u64,
    would_filtered: u64,
}

/// Hit counters for one ruleset, pinned to the ruleset content they were
/// counted against. A recorded/read fingerprint that differs means the rules
/// were replaced → the stale counters read as zero / reset on the next write.
#[derive(Default)]
struct LayerHits {
    fingerprint: u64,
    /// Aligned to the ruleset's rule indices (grown on demand).
    rules: Vec<u64>,
    /// Hits where the ruleset's `default` action decided.
    default_hits: u64,
}

impl LayerHits {
    fn record(&mut self, fingerprint: u64, rule: Option<usize>) {
        if self.fingerprint != fingerprint {
            *self = LayerHits {
                fingerprint,
                ..Default::default()
            };
        }
        match rule {
            Some(i) => {
                if self.rules.len() <= i {
                    self.rules.resize(i + 1, 0);
                }
                self.rules[i] += 1;
            }
            None => self.default_hits += 1,
        }
    }
}

/// Point-in-time per-rule hit counts for one ruleset (all zeroes when the
/// ruleset changed since the counters were recorded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterHits {
    /// Indexed like the ruleset's `rules`; missing tail indices mean zero.
    pub rules: Vec<u64>,
    pub default_hits: u64,
}

/// Process-wide live session stats. Cloneable handle via `Arc`.
#[derive(Default)]
pub struct Stats {
    inner: Mutex<Inner>,
}

/// Per-variation counters at a point in time.
#[derive(Clone, Copy, Default)]
pub struct VarStat {
    pub active: u64,
    pub requests: u64,
    pub filtered: u64,
    pub would_filtered: u64,
}

/// A point-in-time copy of the counters, for the status endpoint to read without
/// holding the live lock while it serializes.
pub struct StatsSnapshot {
    pub active_total: u64,
    pub active_direct: u64,
    pub requests_total: u64,
    pub filtered_total: u64,
    pub would_filtered_total: u64,
    /// Actually bound SOCKS5 address (see [`Stats::set_bound_addr`]).
    pub bound_addr: Option<SocketAddr>,
    per_variation: HashMap<String, VarStat>,
}

impl StatsSnapshot {
    /// Counters for one variation id (zeroes if it has never served a session).
    pub fn variation(&self, id: &str) -> VarStat {
        self.per_variation.get(id).copied().unwrap_or_default()
    }

    /// At least one active session is leaking through a `direct` upstream — the
    /// conservative "amber" signal for the tray icon (any active leak ⇒ warn).
    pub fn any_active_direct(&self) -> bool {
        self.active_direct > 0
    }
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record a session start: bump the active gauges and the cumulative request
    /// count (plus the per-variation counters when the session is bound to one).
    /// The returned guard decrements the *active* gauges on drop (session end);
    /// the cumulative counts stay.
    pub fn begin(self: &Arc<Self>, kind: UpstreamKind, variation: Option<String>) -> SessionGuard {
        let direct = matches!(kind, UpstreamKind::Direct);
        {
            let mut g = self.inner.lock().expect("stats lock");
            g.active_total += 1;
            g.requests_total += 1;
            if direct {
                g.active_direct += 1;
            }
            if let Some(id) = &variation {
                let c = g.per_variation.entry(id.clone()).or_default();
                c.active += 1;
                c.requests += 1;
            }
        }
        SessionGuard {
            stats: self.clone(),
            direct,
            variation,
        }
    }

    /// Record a CONNECT refused by the domain filter. Bumps the cumulative
    /// counter (and the per-variation one when the session was bound to a silo
    /// variation). No session begins, so there is no active gauge to touch.
    pub fn record_filtered(self: &Arc<Self>, variation: Option<&str>) {
        let mut g = self.inner.lock().expect("stats lock");
        g.filtered_total += 1;
        if let Some(id) = variation {
            g.per_variation.entry(id.to_string()).or_default().filtered += 1;
        }
    }

    /// Record a dry-run deny: a `log_only` ruleset produced a `deny` verdict
    /// but the CONNECT goes through. Only these counters remember it —
    /// `filtered_total` stays untouched (nothing was refused).
    pub fn record_would_filtered(self: &Arc<Self>, variation: Option<&str>) {
        let mut g = self.inner.lock().expect("stats lock");
        g.would_filtered_total += 1;
        if let Some(id) = variation {
            g.per_variation
                .entry(id.to_string())
                .or_default()
                .would_filtered += 1;
        }
    }

    /// Attribute one filter verdict to the rule (or default) that produced it.
    /// `fingerprint` pins the counters to the ruleset content: a replaced
    /// ruleset resets its counters on the next record — no explicit reset
    /// plumbing. The silo layer is keyed by variation id (RAM-only, never in
    /// the encrypted blob).
    pub fn record_filter_hit(
        &self,
        layer: Layer,
        variation: Option<&str>,
        fingerprint: u64,
        rule: Option<usize>,
    ) {
        let mut g = self.inner.lock().expect("stats lock");
        match layer {
            Layer::Instance => g.hits_instance.record(fingerprint, rule),
            Layer::Floor => g.hits_floor.record(fingerprint, rule),
            Layer::Silo => {
                let id = variation.unwrap_or_default().to_string();
                g.hits_silo.entry(id).or_default().record(fingerprint, rule);
            }
        }
    }

    /// Read the hit counters for one layer, validated against the *current*
    /// ruleset fingerprint — counters recorded against replaced rules read as
    /// all-zero instead of mislabelling fresh rule indices.
    pub fn filter_hits(
        &self,
        layer: Layer,
        variation: Option<&str>,
        fingerprint: u64,
    ) -> FilterHits {
        let g = self.inner.lock().expect("stats lock");
        let hits = match layer {
            Layer::Instance => Some(&g.hits_instance),
            Layer::Floor => Some(&g.hits_floor),
            Layer::Silo => variation.and_then(|id| g.hits_silo.get(id)),
        };
        match hits {
            Some(h) if h.fingerprint == fingerprint => FilterHits {
                rules: h.rules.clone(),
                default_hits: h.default_hits,
            },
            _ => FilterHits::default(),
        }
    }

    /// Publish the SOCKS5 listener's actually-bound address. Called by the
    /// server task after every successful (re)bind — this is what makes
    /// auto-port mode (`listen.addr` with port `0`) discoverable: the fixed
    /// admin port serves the real port via `GET /v1/status`.
    pub fn set_bound_addr(&self, addr: SocketAddr) {
        self.inner.lock().expect("stats lock").bound_addr = Some(addr);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let g = self.inner.lock().expect("stats lock");
        StatsSnapshot {
            active_total: g.active_total,
            active_direct: g.active_direct,
            requests_total: g.requests_total,
            filtered_total: g.filtered_total,
            would_filtered_total: g.would_filtered_total,
            bound_addr: g.bound_addr,
            per_variation: g
                .per_variation
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        VarStat {
                            active: v.active,
                            requests: v.requests,
                            filtered: v.filtered,
                            would_filtered: v.would_filtered,
                        },
                    )
                })
                .collect(),
        }
    }
}

/// RAII session marker: decrements the active gauges when the session ends.
/// Cumulative counters (`requests_total`, per-variation `requests`) are not
/// touched — they only ever grow.
pub struct SessionGuard {
    stats: Arc<Stats>,
    direct: bool,
    variation: Option<String>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut g = self.stats.inner.lock().expect("stats lock");
        g.active_total = g.active_total.saturating_sub(1);
        if self.direct {
            g.active_direct = g.active_direct.saturating_sub(1);
        }
        if let Some(id) = &self.variation {
            if let Some(c) = g.per_variation.get_mut(id) {
                c.active = c.active.saturating_sub(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_increments_and_guard_decrements() {
        let s = Stats::new();
        let g1 = s.begin(UpstreamKind::HttpConnect, Some("v1".into()));
        let g2 = s.begin(UpstreamKind::HttpConnect, Some("v1".into()));
        let snap = s.snapshot();
        assert_eq!(snap.active_total, 2);
        assert_eq!(snap.requests_total, 2);
        assert_eq!(snap.variation("v1").active, 2);
        assert_eq!(snap.variation("v1").requests, 2);
        drop(g1);
        drop(g2);
        let snap = s.snapshot();
        assert_eq!(snap.active_total, 0);
        assert_eq!(snap.variation("v1").active, 0);
        // Cumulative requests survive session end.
        assert_eq!(snap.requests_total, 2);
        assert_eq!(snap.variation("v1").requests, 2);
    }

    #[test]
    fn direct_session_flags_active_direct() {
        let s = Stats::new();
        assert!(!s.snapshot().any_active_direct());
        let g = s.begin(UpstreamKind::Direct, None);
        assert!(s.snapshot().any_active_direct());
        assert_eq!(s.snapshot().active_direct, 1);
        drop(g);
        // Mixed: one direct + one proxied → still amber while the direct lives.
        let _proxied = s.begin(UpstreamKind::HttpConnect, None);
        assert!(!s.snapshot().any_active_direct());
        let _leak = s.begin(UpstreamKind::Direct, None);
        assert!(s.snapshot().any_active_direct());
    }

    #[test]
    fn unknown_variation_reads_as_zero() {
        let s = Stats::new();
        let v = s.snapshot().variation("never-seen");
        assert_eq!(v.active, 0);
        assert_eq!(v.requests, 0);
    }

    #[test]
    fn filter_hits_count_and_reset_on_fingerprint_change() {
        let s = Stats::new();
        let fp1 = 111u64;
        s.record_filter_hit(Layer::Instance, None, fp1, Some(2));
        s.record_filter_hit(Layer::Instance, None, fp1, Some(2));
        s.record_filter_hit(Layer::Instance, None, fp1, Some(0));
        s.record_filter_hit(Layer::Instance, None, fp1, None);
        let h = s.filter_hits(Layer::Instance, None, fp1);
        assert_eq!(h.rules, vec![1, 0, 2]);
        assert_eq!(h.default_hits, 1);

        // Reading with a fresher fingerprint (rules replaced) → all zeroes.
        let fp2 = 222u64;
        assert_eq!(
            s.filter_hits(Layer::Instance, None, fp2),
            FilterHits::default()
        );
        // Recording against the new fingerprint resets the stale counters.
        s.record_filter_hit(Layer::Instance, None, fp2, Some(0));
        let h = s.filter_hits(Layer::Instance, None, fp2);
        assert_eq!(h.rules, vec![1]);
        assert_eq!(h.default_hits, 0);

        // Layers are independent; silo hits are keyed per variation id.
        s.record_filter_hit(Layer::Silo, Some("v1"), fp1, Some(0));
        assert_eq!(s.filter_hits(Layer::Silo, Some("v1"), fp1).rules, vec![1]);
        assert_eq!(
            s.filter_hits(Layer::Silo, Some("v2"), fp1),
            FilterHits::default()
        );
        assert_eq!(
            s.filter_hits(Layer::Floor, None, fp1),
            FilterHits::default()
        );
    }

    #[test]
    fn would_filtered_counts_apart_from_filtered() {
        let s = Stats::new();
        s.record_would_filtered(Some("v1"));
        s.record_would_filtered(None);
        let snap = s.snapshot();
        assert_eq!(snap.would_filtered_total, 2);
        assert_eq!(snap.filtered_total, 0, "dry-run must not count as refused");
        assert_eq!(snap.variation("v1").would_filtered, 1);
        assert_eq!(snap.variation("v1").filtered, 0);
    }
}
