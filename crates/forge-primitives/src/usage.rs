//! Anthropic plan-usage snapshot data shapes.
//!
//! Type-only  -  the fetcher impl (HTTP via `oauth_usage.rs`) lives in
//! `forge_agent::cloud::*`. These are the wire shapes consumers see.
//!
//! Serde derived so the workspace can persist the latest snapshot per
//! account to disk and rehydrate it at next boot  -  without the cache
//! the launchpad shows empty bars until the live `/api/oauth/usage`
//! probe lands, which Anthropic's per-IP rate limiter can stall for
//! 30 s+.

use serde::{Deserialize, Serialize};

pub mod oauth;

/// Origin of a [`UsageSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageSourceKind {
    Oauth,
}

impl UsageSourceKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
        }
    }
}

/// One named usage window inside a snapshot (5-hour, 7-day, etc.).
/// The window's identity (5-hour vs 7-day vs 7-day-opus) is implicit
/// in which `UsageSnapshot` field holds the value - no in-struct label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub utilization: f64,
    pub resets_at: Option<std::time::SystemTime>,
    pub reset_description: Option<String>,
}

impl UsageWindow {
    /// True when this window is currently at or beyond its plan cap
    /// AND the reset time has not yet passed. The pair matters: a
    /// 100% utilization figure from a cached snapshot remains at 100%
    /// after the reset window expires (the cache is stale by then),
    /// so the predicate must consult `resets_at` to avoid forever
    /// classifying an account as rate-limited based on a stale probe.
    ///
    /// `resets_at == None` returns `false`: a window with no scheduled
    /// reset cannot transition out of the limit, so treating it as
    /// permanently limited would strand the account. The usage probe
    /// emits `resets_at = Some(...)` whenever a window is real; the
    /// `None` case shows up in tests + edge-case snapshots only.
    pub fn is_currently_limited(&self) -> bool {
        if self.utilization < 100.0 {
            return false;
        }
        self.resets_at.is_some_and(|when| when > std::time::SystemTime::now())
    }
}

/// Bonus / overage credits view returned alongside the per-window
/// utilization figures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtraUsage {
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
}

/// Snapshot of the user's Anthropic plan utilization at a point in time.
/// Composed by the cloud module's `oauth` fetcher and rendered by
/// forge-tui's usage view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub source: UsageSourceKind,
    pub fetched_at: std::time::SystemTime,
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub seven_day_opus: Option<UsageWindow>,
    pub seven_day_sonnet: Option<UsageWindow>,
    pub extra_usage: Option<ExtraUsage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn window(utilization: f64, resets_at: Option<SystemTime>) -> UsageWindow {
        UsageWindow { utilization, resets_at, reset_description: None }
    }

    #[test]
    fn is_currently_limited_true_when_at_cap_with_future_reset() {
        let future = SystemTime::now() + Duration::from_secs(60);
        assert!(window(100.0, Some(future)).is_currently_limited());
    }

    #[test]
    fn is_currently_limited_true_when_above_cap_with_future_reset() {
        // utilization > 100% (e.g. server reports 101% during a brief
        // overshoot) still counts as limited as long as the reset window
        // is in the future.
        let future = SystemTime::now() + Duration::from_secs(60);
        assert!(window(101.0, Some(future)).is_currently_limited());
    }

    #[test]
    fn is_currently_limited_false_when_at_cap_but_reset_passed() {
        // Stale cached window: probe reported 100% an hour ago, the
        // reset moment has come and gone but no fresh probe has yet
        // overwritten the cache. Predicate must return false so the
        // renderer stops painting the rate-limit label.
        let past = SystemTime::now() - Duration::from_secs(60);
        assert!(!window(100.0, Some(past)).is_currently_limited());
    }

    #[test]
    fn is_currently_limited_false_when_below_cap() {
        let future = SystemTime::now() + Duration::from_secs(60);
        assert!(!window(99.9, Some(future)).is_currently_limited());
    }

    #[test]
    fn is_currently_limited_false_when_resets_at_missing() {
        // No scheduled reset means the predicate cannot prove the
        // limit is current. Returning true would strand the account
        // as forever-limited based on a single cached snapshot.
        assert!(!window(100.0, None).is_currently_limited());
    }
}
