//! Anthropic plan-usage snapshot data shapes.
//!
//! Type-only - the fetcher impls live in the forge-providers
//! backends. These are the wire shapes consumers see.
//!
//! Serde derived so the workspace can persist the latest snapshot per
//! account to disk and rehydrate it at next boot - without the cache
//! the launchpad shows empty bars until the live `/api/oauth/usage`
//! probe lands, which Anthropic's per-IP rate limiter can stall for
//! 30 s+.

use serde::{Deserialize, Serialize};

pub mod oauth;
pub mod openrouter;
pub mod zai;

/// Origin of a [`UsageSnapshot`], and with it which half of the
/// snapshot carries data: `Oauth` and `ZaiMonitor` fill the windows,
/// `OpenRouterKey` fills [`UsageSnapshot::spend`].
///
/// Load-bearing across a config change: the snapshot is cached and
/// rehydrated, so an account whose `provider` was edited still has a
/// row in the old shape. A renderer that compares this against the
/// account's declared provider can fall back to "no data" instead of
/// reading windows as money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageSourceKind {
    Oauth,
    OpenRouterKey,
    ZaiMonitor,
}

impl UsageSourceKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::OpenRouterKey => "openrouter-key",
            Self::ZaiMonitor => "zai-monitor",
        }
    }
}

/// Per-key spend for a pay-per-token account, in USD.
///
/// Not [`ExtraUsage`], which is Anthropic overage: that type's money
/// fields arrive in minor units and are divided by 100 on the way in,
/// and it carries a `utilization` percentage. These are decimal USD
/// straight off the wire, and an uncapped key has no denominator to be
/// a percentage of.
///
/// Every figure is scoped to one key. Account-wide balance comes from a
/// different endpoint with a different scope and is deliberately absent
/// so a row cannot imply both are per-key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiSpend {
    pub daily: f64,
    pub weekly: f64,
    pub monthly: f64,
    /// Spending cap on this key, in USD, with what is left of it and
    /// the cadence it resets on. All `None` on an uncapped key, which
    /// is a normal state - a cap can be added or removed from the
    /// provider's dashboard between one poll and the next, so nothing
    /// may assume a denominator exists.
    pub limit: Option<f64>,
    pub limit_remaining: Option<f64>,
    pub limit_reset: Option<String>,
    /// When the key stops working, when it says.
    pub expires_at: Option<String>,
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
    /// Per-key spend for an API-billed account. `None` for every
    /// window-billed source, and for every row written before this
    /// field existed - serde decodes a missing key on an `Option` to
    /// `None`, which is what keeps the cached rows readable.
    pub spend: Option<ApiSpend>,
}

impl UsageSnapshot {
    /// `None` when the snapshot carries no five-hour window, which is a
    /// documented steady state on the lenient mapper rather than a zero.
    pub fn five_hour_util(&self) -> Option<f64> {
        self.five_hour.as_ref().map(|w| w.utilization)
    }

    /// Binding 7-day utilisation: max across the three 7-day windows
    /// (`seven_day`, `seven_day_opus`, `seven_day_sonnet`). Whichever
    /// is most-used is the binding constraint for "is this account
    /// 7-day rate-limited."
    ///
    /// `None` when all three are absent, which a 200 carrying only the
    /// session window produces.
    pub fn seven_day_util(&self) -> Option<f64> {
        let windows = [
            self.seven_day.as_ref().map(|w| w.utilization),
            self.seven_day_opus.as_ref().map(|w| w.utilization),
            self.seven_day_sonnet.as_ref().map(|w| w.utilization),
        ];
        windows.into_iter().flatten().reduce(f64::max)
    }

    /// When a rate-limited account unlocks: the latest `resets_at` among
    /// windows currently at-or-over the cap (per `is_currently_limited`).
    /// `None` when no window is currently capped, so the `/account`
    /// picker shows a reset ETA only on rate-limited rows.
    pub fn binding_reset_at(&self) -> Option<std::time::SystemTime> {
        [
            self.five_hour.as_ref(),
            self.seven_day.as_ref(),
            self.seven_day_opus.as_ref(),
            self.seven_day_sonnet.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|w| w.is_currently_limited())
        .filter_map(|w| w.resets_at)
        .max()
    }
}

/// What an account has left, in the terms its backend bills in.
///
/// A view over a [`UsageSnapshot`], deliberately not part of it: the
/// snapshot is persisted and this is not, so the stored type stays a
/// struct and serde keeps decoding every cached row.
#[derive(Clone, Debug, PartialEq)]
pub enum AccountBudget {
    /// No usable snapshot: none has landed yet, or the cached one was
    /// written under a different `provider` and no longer describes
    /// this account.
    ///
    /// `spend_billed` carries the account's billing model anyway, so
    /// the row's empty columns sit under the labels it would really
    /// have rather than asserting windows an API account has none of.
    Unknown { spend_billed: bool },
    /// Plan windows, as percentages of an allowance that resets. Each
    /// column is `None` when the snapshot carried no window for it -
    /// the lenient mapper documents three states where that happens,
    /// and the strict one requires only the five-hour window, so a
    /// present snapshot is not a promise of a present figure.
    Subscription {
        five_hour_util: Option<f64>,
        /// Binding 7-day utilization: max across the three 7-day
        /// windows, or `None` when all three are absent.
        seven_day_util: Option<f64>,
        /// When the account unlocks - `Some` only while it is at its
        /// cap, so the picker shows a reset ETA on limited rows only.
        resets_at: Option<std::time::SystemTime>,
    },
    /// Per-key spend in USD over the three periods the backend
    /// pre-computes. No allowance, so no percentage and no reset;
    /// account-wide balance has a different scope and is not carried
    /// here, so a row cannot imply both figures are per-key.
    Api { daily: f64, weekly: f64, monthly: f64 },
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
