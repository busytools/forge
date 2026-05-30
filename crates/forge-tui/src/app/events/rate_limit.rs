use super::super::{App, NoticeDedupKey, NoticeStage, RateLimitIncidentKey, SystemSeverity};
use crate::agent::model;
use std::time::Duration;

const EXTRA_USAGE_REQUIRED_MESSAGE: &str = "Extra usage credit is required to continue. Use /extra-usage to enable it, /model to switch models, or wait for the rate-limit window to reset.";

fn format_rate_limit_type(raw: &str) -> &str {
    match raw {
        "five_hour" => "5-hour",
        "daily" => "daily",
        "minute" => "per-minute",
        "seven_day" => "7-day",
        "seven_day_opus" => "7-day Opus",
        "seven_day_sonnet" => "7-day Sonnet",
        "overage" => "overage",
        other => other,
    }
}

/// Format an epoch timestamp as a countdown and UTC wall-clock: "4h 23m at 14:30 UTC".
fn format_resets_at(epoch_secs: f64) -> String {
    use std::time::{Duration, UNIX_EPOCH};

    // `Duration::from_secs_f64` panics on negative, NaN, or infinite  -
    // sibling fns in this file (`reset_bucket_from_epoch_secs`,
    // `maybe_recover_from_rate_limit_lock`) guard the same shape;
    // wire `RateLimitInfo.resetsAt` only filters `is_finite()`,
    // so a negative finite `resetsAt` (clock skew, CLI bug,
    // integer underflow) would crash the TUI process.
    if !epoch_secs.is_finite() || epoch_secs < 0.0 {
        return "now".to_owned();
    }

    let now = std::time::SystemTime::now();

    let countdown = match (UNIX_EPOCH + Duration::from_secs_f64(epoch_secs)).duration_since(now) {
        Ok(d) => {
            let total_secs = d.as_secs();
            if total_secs < 60 {
                "< 1 minute".to_owned()
            } else {
                let hours = total_secs / 3600;
                let minutes = (total_secs % 3600) / 60;
                if hours > 0 { format!("{hours}h {minutes}m") } else { format!("{minutes}m") }
            }
        }
        Err(_) => "now".to_owned(),
    };

    // Wire-side epoch (seconds since UNIX epoch) is f64. `.max(0.0)`
    // above guarantees non-negative; truncation to u64 is intentional
    // for the HH:MM UTC field math below.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let epoch_u64 = epoch_secs.max(0.0) as u64;
    let h = (epoch_u64 % 86400) / 3600;
    let m = (epoch_u64 % 3600) / 60;

    format!("{countdown} at {h:02}:{m:02} UTC")
}

fn has_primary_rate_limit_context(update: &model::RateLimitUpdate) -> bool {
    update.utilization.is_some() || update.rate_limit_type.is_some() || update.resets_at.is_some()
}

fn is_org_level_disabled_extra_usage_case(update: &model::RateLimitUpdate) -> bool {
    matches!(update.status, model::RateLimitStatus::Rejected)
        && !has_primary_rate_limit_context(update)
        && update.is_using_overage == Some(false)
        && update.overage_disabled_reason.as_deref() == Some("org_level_disabled")
}

/// True when the wire reports the user has crossed an overage
/// threshold but Anthropic is not yet billing overage. In this state
/// the warning chip's loud "Approaching rate limit, you've used 102%"
/// over-states things  -  the user is at the threshold, not actually
/// burning overage credit. Detected by `surpassed_threshold > 0`
/// (Anthropic's signal that some threshold was crossed) plus
/// `is_using_overage == Some(false)` (no actual overage consumption).
fn is_near_threshold_without_overage(update: &model::RateLimitUpdate) -> bool {
    matches!(update.status, model::RateLimitStatus::AllowedWarning)
        && update.is_using_overage == Some(false)
        && update.surpassed_threshold.is_some_and(|t| t > 0.0)
}

pub(super) fn format_rate_limit_summary(update: &model::RateLimitUpdate) -> String {
    if is_org_level_disabled_extra_usage_case(update) {
        return EXTRA_USAGE_REQUIRED_MESSAGE.to_owned();
    }

    if is_near_threshold_without_overage(update) {
        // Drop the percentage and the "you can continue using overage"
        // tail  -  the user isn't actually consuming overage credit, so
        // the loud wording isn't warranted. Keep the reset time, which
        // is the only actionable bit.
        let mut message = "Near rate-limit threshold.".to_owned();
        if let Some(resets_at) = update.resets_at {
            use std::fmt::Write;
            let _ = write!(message, " Resets in {}.", format_resets_at(resets_at));
        }
        return message;
    }

    let is_rejected = matches!(update.status, model::RateLimitStatus::Rejected);

    // Intro
    let intro = if is_rejected { "Rate limit reached" } else { "Approaching rate limit" };

    // "you've used 91% of your 5-hour rate limit"
    let usage_part = match (update.utilization, &update.rate_limit_type) {
        (Some(util), Some(rlt)) => {
            format!(
                "you've used {:.0}% of your {} rate limit",
                util * 100.0,
                format_rate_limit_type(rlt),
            )
        }
        (Some(util), None) => format!("you've used {:.0}% of your rate limit", util * 100.0),
        (None, Some(rlt)) => {
            format!("you've hit your {} rate limit", format_rate_limit_type(rlt))
        }
        (None, None) => "you've hit your rate limit".to_owned(),
    };

    let mut message = format!("{intro}, {usage_part}.");

    // Overage hint
    if is_rejected {
        // Rejected: state if overage is in use
        if update.is_using_overage == Some(true) {
            message.push_str(" You are using your overage allowance.");
        }
    } else {
        // Warning: hint that overage is available
        if update.is_using_overage == Some(false) || update.overage_status.is_some() {
            message.push_str(" You can continue using your overage allowance.");
        }
    }

    // Resets in X at HH:MM
    if let Some(resets_at) = update.resets_at {
        use std::fmt::Write;
        let _ = write!(message, " Resets in {}.", format_resets_at(resets_at));
    }

    message
}

pub(super) fn rate_limit_notice_key(update: &model::RateLimitUpdate) -> NoticeDedupKey {
    NoticeDedupKey::RateLimit(RateLimitIncidentKey {
        rate_limit_type: update.rate_limit_type.clone(),
        resets_at_bucket: update.resets_at.and_then(reset_bucket_from_epoch_secs),
    })
}

fn reset_bucket_from_epoch_secs(value: f64) -> Option<u64> {
    if !value.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(value.max(0.0)).as_secs())
}

pub(super) fn handle_rate_limit_update(app: &mut App, update: &model::RateLimitUpdate) {
    app.set_last_rate_limit_update(Some(update.clone()));
    tracing::debug!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "rate_limit_update_applied",
        message = "rate limit update applied",
        outcome = "success",
        status = ?update.status,
        utilization = update.utilization,
        rate_limit_type = update.rate_limit_type.as_deref().unwrap_or(""),
        resets_at = update.resets_at.unwrap_or_default(),
        overage_status = ?update.overage_status,
        overage_resets_at = update.overage_resets_at.unwrap_or_default(),
        overage_disabled_reason = update.overage_disabled_reason.as_deref().unwrap_or(""),
        is_using_overage = ?update.is_using_overage,
        surpassed_threshold = update.surpassed_threshold.unwrap_or_default(),
    );

    match update.status {
        model::RateLimitStatus::Allowed => {}
        model::RateLimitStatus::AllowedWarning => {
            let summary = format_rate_limit_summary(update);
            super::notices::upsert_turn_notice(
                app,
                rate_limit_notice_key(update),
                NoticeStage::Warning,
                SystemSeverity::Warning,
                &summary,
            );
        }
        model::RateLimitStatus::Rejected => {
            let summary = format_rate_limit_summary(update);
            super::notices::upsert_turn_notice(
                app,
                rate_limit_notice_key(update),
                NoticeStage::Rejected,
                SystemSeverity::Error,
                &summary,
            );
        }
    }
}

/// Drop the input lock after a rate-limit-induced Error state once
/// the wall clock has crossed `last_rate_limit_update.resets_at`.
/// Called once per main-loop tick. Without this, the App stays
/// locked until the user kills the binary even after the rate-limit
/// window has actually passed.
pub(crate) fn maybe_recover_from_rate_limit_lock(app: &mut App) {
    use super::super::AppStatus;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    if !matches!(app.status, AppStatus::Error) {
        return;
    }
    let (status, resets_at) = match app.last_rate_limit_update() {
        Some(update) => (update.status, update.resets_at),
        None => return,
    };
    if !matches!(status, model::RateLimitStatus::Rejected) {
        return;
    }
    let Some(resets_at) = resets_at else {
        return;
    };
    if !resets_at.is_finite() || resets_at <= 0.0 {
        return;
    }
    let reset_target = UNIX_EPOCH + Duration::from_secs_f64(resets_at);
    if SystemTime::now() < reset_target {
        return;
    }
    // Window has passed  -  recover.
    app.set_last_rate_limit_update(None);
    app.status = AppStatus::Ready;
    app.exit_error = None;
    super::notices::clear_turn_notice_tracking(app);
    super::push_system_message_with_severity(
        app,
        Some(SystemSeverity::Info),
        "Rate-limit window passed  -  input re-enabled. You can retry your request.",
    );
    app.needs_redraw = true;
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "rate_limit_lock_auto_recovered",
        message = "rate-limit reset window passed; input re-enabled",
        outcome = "success",
    );
}

pub(crate) fn handle_compaction_boundary_update(
    app: &mut App,
    boundary: model::CompactionBoundary,
) {
    app.set_is_compacting(true);
    if matches!(boundary.trigger, model::CompactionTrigger::Manual) {
        app.set_pending_compact_clear(true);
    }
    let usage = app.session_usage_mut();
    usage.last_compaction_trigger = Some(boundary.trigger);
    usage.last_compaction_pre_tokens = Some(boundary.pre_tokens);
    tracing::debug!(
        "CompactionBoundary: trigger={:?} pre_tokens={}",
        boundary.trigger,
        boundary.pre_tokens
    );
}

#[cfg(test)]
mod tests {
    use super::{format_rate_limit_summary, format_resets_at};
    use crate::agent::model::{RateLimitStatus, RateLimitUpdate};

    #[test]
    fn format_resets_at_returns_now_for_negative_nan_infinite_epoch() {
        // `Duration::from_secs_f64` panics on these  -  the guard
        // returns the literal "now" instead of crashing the TUI.
        assert_eq!(format_resets_at(-1.0), "now");
        assert_eq!(format_resets_at(-1e9), "now");
        assert_eq!(format_resets_at(f64::NAN), "now");
        assert_eq!(format_resets_at(f64::INFINITY), "now");
        assert_eq!(format_resets_at(f64::NEG_INFINITY), "now");
    }

    #[test]
    fn org_level_disabled_without_primary_context_uses_extra_usage_message() {
        let update = RateLimitUpdate {
            status: RateLimitStatus::Rejected,
            resets_at: None,
            utilization: None,
            rate_limit_type: None,
            overage_status: None,
            overage_resets_at: None,
            overage_disabled_reason: Some("org_level_disabled".to_owned()),
            is_using_overage: Some(false),
            surpassed_threshold: None,
        };

        assert_eq!(
            format_rate_limit_summary(&update),
            "Extra usage credit is required to continue. Use /extra-usage to enable it, /model to switch models, or wait for the rate-limit window to reset."
        );
    }

    #[test]
    fn org_level_disabled_with_primary_context_keeps_normal_rate_limit_message() {
        let update = RateLimitUpdate {
            status: RateLimitStatus::Rejected,
            resets_at: Some(1_741_280_000.0),
            utilization: None,
            rate_limit_type: Some("five_hour".to_owned()),
            overage_status: None,
            overage_resets_at: None,
            overage_disabled_reason: Some("org_level_disabled".to_owned()),
            is_using_overage: Some(false),
            surpassed_threshold: None,
        };

        let summary = format_rate_limit_summary(&update);
        assert!(summary.contains("Rate limit reached"));
        assert!(summary.contains("5-hour rate limit"));
        assert!(!summary.contains("Extra usage is required for 1M context"));
    }

    #[test]
    fn near_threshold_without_overage_uses_softer_wording() {
        // AllowedWarning + surpassedThreshold > 0 + isUsingOverage =
        // false: user is past the threshold but not actually consuming
        // overage  -  the loud "you've used 102%" wording over-states.
        let update = RateLimitUpdate {
            status: RateLimitStatus::AllowedWarning,
            resets_at: Some(1_741_280_000.0),
            utilization: Some(1.02),
            rate_limit_type: Some("overage".to_owned()),
            overage_status: None,
            overage_resets_at: None,
            overage_disabled_reason: None,
            is_using_overage: Some(false),
            surpassed_threshold: Some(1.0),
        };

        let summary = format_rate_limit_summary(&update);
        assert!(summary.starts_with("Near rate-limit threshold"));
        assert!(summary.contains("Resets in"));
        // No percentage  -  that's the whole point of softening.
        assert!(!summary.contains('%'));
        // No "you can continue" overage hint  -  user isn't consuming
        // overage, so the hint is misleading.
        assert!(!summary.contains("overage allowance"));
    }

    #[test]
    fn near_threshold_when_overage_in_use_keeps_loud_wording() {
        // Same surpassed_threshold but actually using overage  -  keep
        // the loud "Approaching rate limit, X%" message because the
        // user genuinely wants to know.
        let update = RateLimitUpdate {
            status: RateLimitStatus::AllowedWarning,
            resets_at: Some(1_741_280_000.0),
            utilization: Some(1.05),
            rate_limit_type: Some("overage".to_owned()),
            overage_status: None,
            overage_resets_at: None,
            overage_disabled_reason: None,
            is_using_overage: Some(true),
            surpassed_threshold: Some(1.0),
        };

        let summary = format_rate_limit_summary(&update);
        assert!(summary.contains("Approaching rate limit"));
        assert!(summary.contains("105%") || summary.contains("100%"));
    }
}
