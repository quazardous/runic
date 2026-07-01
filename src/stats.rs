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
use std::sync::{Arc, Mutex};

use crate::config::UpstreamKind;

#[derive(Default)]
struct Inner {
    active_total: u64,
    /// Active sessions whose chosen upstream is `kind: direct` (local IP exposed).
    active_direct: u64,
    requests_total: u64,
    /// Cumulative CONNECTs refused by the domain filter (never begins a session).
    filtered_total: u64,
    per_variation: HashMap<String, VarCounters>,
}

#[derive(Default, Clone, Copy)]
struct VarCounters {
    active: u64,
    requests: u64,
    filtered: u64,
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
}

/// A point-in-time copy of the counters, for the status endpoint to read without
/// holding the live lock while it serializes.
pub struct StatsSnapshot {
    pub active_total: u64,
    pub active_direct: u64,
    pub requests_total: u64,
    pub filtered_total: u64,
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

    pub fn snapshot(&self) -> StatsSnapshot {
        let g = self.inner.lock().expect("stats lock");
        StatsSnapshot {
            active_total: g.active_total,
            active_direct: g.active_direct,
            requests_total: g.requests_total,
            filtered_total: g.filtered_total,
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
}
