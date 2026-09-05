use std::time::SystemTime;

use crate::cloud::{UsageSnapshot, UsageSourceKind, UsageWindow};
use forge_providers::helpers::system_time_from_epoch;

/// Why a probe payload could not be mapped to a
/// [`forge_primitives::usage::UsageSnapshot`]. Only the zai mapper
/// below produces this; the windowed mappers live in forge-providers
/// now.
#[derive(Debug)]
pub enum OauthFetchError {
    Failed(String),
}

/// Map a Z.ai quota-limit payload into a
/// [`forge_primitives::usage::UsageSnapshot`].
///
/// CREDIT_LIMIT entries carry the windows in credits: `usage` is the
/// per-window limit and consumption is `usage - remaining`, which is
/// where the utilization percentage comes from - the payload's own
/// `percentage` field is not mapped. The unit-3 (hours) entry is the
/// 5-hour window, unit-6 (weeks) the weekly one. An absent 5-hour
/// `nextResetTime`, the steady state before the first successful
/// request, maps to a window with no reset moment.
///
/// Fallible like [`snapshot_from_openrouter_key`]: a payload with no
/// mappable window entries is a response forge cannot read rather
/// than a bill of zero.
pub fn snapshot_from_zai_quota(
    payload: forge_primitives::usage::zai::QuotaLimitData,
) -> Result<UsageSnapshot, OauthFetchError> {
    let mut five_hour = None;
    let mut seven_day = None;
    for entry in payload.limits {
        if entry.kind.as_deref() != Some("CREDIT_LIMIT") {
            continue;
        }
        match entry.unit {
            Some(3) => five_hour = zai_window_from_entry(&entry),
            Some(6) => seven_day = zai_window_from_entry(&entry),
            _ => {}
        }
    }
    if five_hour.is_none() && seven_day.is_none() {
        return Err(OauthFetchError::Failed(
            "Z.ai quota response carried no CREDIT_LIMIT window entries.".to_owned(),
        ));
    }
    Ok(UsageSnapshot {
        source: UsageSourceKind::ZaiMonitor,
        fetched_at: SystemTime::now(),
        five_hour,
        seven_day,
        seven_day_opus: None,
        seven_day_sonnet: None,
        extra_usage: None,
        spend: None,
    })
}

fn zai_window_from_entry(
    entry: &forge_primitives::usage::zai::QuotaLimitEntry,
) -> Option<UsageWindow> {
    let usage = entry.usage?;
    let remaining = entry.remaining?;
    let utilization = if usage > 0.0 {
        ((usage - remaining).max(0.0) / usage * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    Some(UsageWindow {
        utilization,
        resets_at: entry.next_reset_time.and_then(system_time_from_epoch),
        reset_description: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verified after-usage shape maps credit arithmetic to window
    /// percentages: utilization is `usage - remaining` against `usage`,
    /// and `nextResetTime` epoch milliseconds become the window's
    /// reset moment.
    #[test]
    fn zai_quota_maps_credit_windows_to_percentages() {
        let payload: forge_primitives::usage::zai::QuotaLimitData = serde_json::from_str(
            r#"{
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,
                     "remaining":27104,"percentage":3.2,"currentValue":0,
                     "nextResetTime":1757025600000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":139000,"percentage":0.71,"currentValue":0,
                     "nextResetTime":1757000000000}
                ],
                "level": "max"
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_zai_quota(payload).expect("maps");
        assert_eq!(snapshot.source, UsageSourceKind::ZaiMonitor);
        let five = snapshot.five_hour.expect("5h window");
        assert!(
            (five.utilization - 3.2).abs() < 1e-9,
            "896 of 28000 credits is 3.2%, got {}",
            five.utilization,
        );
        assert_eq!(
            five.resets_at,
            // 1757025600000 ms on the wire; the same instant in seconds.
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_757_025_600)),
            "epoch-ms nextResetTime becomes the reset moment",
        );
        let weekly = snapshot.seven_day.expect("weekly window");
        assert!(
            (weekly.utilization - 1000.0 / 1400.0).abs() < 1e-9,
            "1000 of 140000 credits, got {}",
            weekly.utilization,
        );
        assert!(snapshot.spend.is_none(), "a subscription carries no per-key spend");
    }

    /// A fresh account has consumed nothing and the 5-hour entry has
    /// no `nextResetTime` yet - that maps to a zero window with no
    /// reset moment, not an error and not a fabricated reset.
    #[test]
    fn zai_quota_maps_a_fresh_account_with_no_five_hour_reset() {
        let payload: forge_primitives::usage::zai::QuotaLimitData = serde_json::from_str(
            r#"{
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,
                     "remaining":28000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":140000,"nextResetTime":1757000000000}
                ],
                "level": "max"
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_zai_quota(payload).expect("maps");
        let five = snapshot.five_hour.expect("5h window");
        assert!(five.utilization.abs() < f64::EPSILON, "fresh account has consumed nothing");
        assert_eq!(five.resets_at, None, "no nextResetTime means no reset moment yet");
    }

    /// An entry with `remaining` absent is an unreadable half-entry,
    /// not a full one: it is skipped like a missing `usage`, because
    /// asserting a default would render a saturated red row off a field
    /// the payload never carried.
    #[test]
    fn zai_quota_skips_an_entry_without_remaining() {
        let payload: forge_primitives::usage::zai::QuotaLimitData = serde_json::from_str(
            r#"{
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":139000,"nextResetTime":1757000000000}
                ],
                "level": "max"
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_zai_quota(payload).expect("the weekly entry maps");
        assert!(
            snapshot.five_hour.is_none(),
            "an entry with no remaining must not map to 100% utilization",
        );
        assert!(snapshot.seven_day.is_some(), "its present sibling still maps");
    }

    /// Both an empty `limits` array and entries of some future
    /// non-CREDIT_LIMIT kind leave no mappable window: that is a
    /// response forge cannot read, not a bill of zero.
    #[test]
    fn zai_quota_with_no_mappable_entries_is_an_error_not_zero() {
        let empty: forge_primitives::usage::zai::QuotaLimitData =
            serde_json::from_str(r#"{"limits":[],"level":"max"}"#).expect("decode");
        assert!(snapshot_from_zai_quota(empty).is_err(), "no windows must not read as a zero bill");
        let foreign_kind: forge_primitives::usage::zai::QuotaLimitData = serde_json::from_str(
            r#"{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"usage":1,"remaining":1}]}"#,
        )
        .expect("decode");
        assert!(
            snapshot_from_zai_quota(foreign_kind).is_err(),
            "a non-CREDIT_LIMIT entry must not become a window",
        );
    }
}

