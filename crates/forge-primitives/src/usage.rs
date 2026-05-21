//! Anthropic plan-usage snapshot data shapes.
//!
//! Type-only — the fetcher impl (HTTP via `oauth_usage.rs`) lives in
//! `forge_agent::cloud::*`. These are the wire shapes consumers see.
//!
//! Serde derived so the workspace can persist the latest snapshot per
//! account to disk and rehydrate it at next boot — without the cache
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// Window name used by the mapper at construction; not read by
    /// the renderer (the window's identity is implicit in which
    /// `UsageSnapshot` field holds it). Skipped during cache round-
    /// trip so the lifetime stays `&'static`.
    #[serde(skip, default = "empty_static_str")]
    pub label: &'static str,
    pub utilization: f64,
    pub resets_at: Option<std::time::SystemTime>,
    pub reset_description: Option<String>,
}

fn empty_static_str() -> &'static str {
    ""
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
