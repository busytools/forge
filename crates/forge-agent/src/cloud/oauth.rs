use std::time::SystemTime;

use crate::cloud::{UsageSnapshot, UsageSourceKind, UsageWindow};
use forge_primitives::usage::ApiSpend;
use forge_providers::helpers::system_time_from_epoch;

/// Why a probe payload could not be mapped to a
/// [`forge_primitives::usage::UsageSnapshot`]. Only the openrouter and
/// zai mappers below produce this; the windowed mappers live in
/// forge-providers now.
#[derive(Debug)]
pub enum OauthFetchError {
    Failed(String),
}

/// Map an OpenRouter `/api/v1/key` payload into a
/// [`forge_primitives::usage::UsageSnapshot`].
///
/// Window-free: a pay-per-token key has no plan window and, when
/// uncapped, no denominator, so nothing here synthesises a utilization.
///
/// Fallible on purpose. A 200 whose body carries no `data` envelope, or
/// an envelope with none of the three usage figures, is a response
/// forge cannot read rather than a bill of zero - and since `set_usage`
/// takes any snapshot to `Ready` without inspecting it, mapping those
/// to zeroes would report a confident number nothing prompts anyone to
/// doubt. An absent figure alongside a present sibling is a real zero
/// and maps as one.
pub fn snapshot_from_openrouter_key(
    payload: forge_primitives::usage::openrouter::KeyResponse,
) -> Result<UsageSnapshot, OauthFetchError> {
    let Some(data) = payload.data else {
        return Err(OauthFetchError::Failed(
            "OpenRouter key response carried no data envelope.".to_owned(),
        ));
    };
    if data.usage_daily.is_none() && data.usage_weekly.is_none() && data.usage_monthly.is_none() {
        return Err(OauthFetchError::Failed(
            "OpenRouter key response carried no usage figures.".to_owned(),
        ));
    }
    Ok(UsageSnapshot {
        source: UsageSourceKind::OpenRouterKey,
        fetched_at: SystemTime::now(),
        five_hour: None,
        seven_day: None,
        seven_day_opus: None,
        seven_day_sonnet: None,
        extra_usage: None,
        spend: Some(ApiSpend {
            daily: data.usage_daily.unwrap_or(0.0),
            weekly: data.usage_weekly.unwrap_or(0.0),
            monthly: data.usage_monthly.unwrap_or(0.0),
            // Carried through as-is: a cap that is absent stays absent
            // rather than becoming a zero, because zero is a cap that
            // permits nothing and absent is a key with no cap at all.
            limit: data.limit,
            limit_remaining: data.limit_remaining,
            limit_reset: data.limit_reset,
            expires_at: data.expires_at,
        }),
    })
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

    /// An uncapped key reports `limit: null`, so there is no
    /// denominator and no honest percentage. The mapper must carry
    /// spend and leave every window empty rather than inventing a 0%
    /// that forge's own saturation predicate would then read.
    #[test]
    fn openrouter_key_maps_to_spend_with_no_windows() {
        let payload: forge_primitives::usage::openrouter::KeyResponse = serde_json::from_str(
            r#"{"data":{
                "label":"sk-or-v1-TEST...TEST",
                "limit":null,"limit_reset":null,"limit_remaining":null,
                "usage":198.552152461,
                "usage_daily":0.5632267,
                "usage_weekly":1.25,
                "usage_monthly":20.296155711,
                "byok_usage":0.000365,
                "is_free_tier":false
            }}"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_openrouter_key(payload).expect("a real payload maps");

        assert_eq!(snapshot.source, UsageSourceKind::OpenRouterKey);
        let spend = snapshot.spend.expect("spend is populated");
        assert!(
            (spend.daily - 0.563_226_7).abs() < f64::EPSILON,
            "daily spend comes straight off usage_daily",
        );
        assert!((spend.weekly - 1.25).abs() < f64::EPSILON, "weekly spend maps");
        assert!((spend.monthly - 20.296_155_711).abs() < f64::EPSILON, "monthly spend maps");
        assert!(
            snapshot.five_hour.is_none()
                && snapshot.seven_day.is_none()
                && snapshot.seven_day_opus.is_none()
                && snapshot.seven_day_sonnet.is_none(),
            "an API-billed account has no plan window to fabricate",
        );
        assert!(
            snapshot.extra_usage.is_none(),
            "extra_usage is Anthropic overage in minor units, not this",
        );
    }

    /// A 200 whose body forge cannot read is not a zero bill. Both
    /// shapes below used to decode to a confident (0.0, 0.0, 0.0) and
    /// take the account Ready, with the only log line saying success.
    #[test]
    fn an_unreadable_openrouter_body_is_an_error_not_zero_spend() {
        let no_envelope: forge_primitives::usage::openrouter::KeyResponse =
            serde_json::from_str(r#"{"error":{"message":"User not found.","code":401}}"#)
                .expect("decodes structurally");
        assert!(
            snapshot_from_openrouter_key(no_envelope).is_err(),
            "a body with no data envelope carries no spend and must not read as zero",
        );

        let no_figures: forge_primitives::usage::openrouter::KeyResponse =
            serde_json::from_str(r#"{"data":{"label":"sk-or-v1-TEST","is_free_tier":false}}"#)
                .expect("decodes structurally");
        assert!(
            snapshot_from_openrouter_key(no_figures).is_err(),
            "an envelope with none of the three usage figures must not read as zero",
        );
    }

    /// A cap can be added or removed from the provider's dashboard
    /// between polls, so both shapes have to map: the capped key
    /// carries a denominator the panel can draw a bar against, the
    /// uncapped one carries none and must not be given a synthesised
    /// zero to stand in for it.
    #[test]
    fn a_cap_maps_when_present_and_stays_absent_when_not() {
        let capped: forge_primitives::usage::openrouter::KeyResponse = serde_json::from_str(
            r#"{"data":{"usage_daily":0.038869563,"usage_weekly":0.038869563,
                        "usage_monthly":0.038869563,"limit":20,
                        "limit_remaining":19.961130437,"limit_reset":"monthly",
                        "expires_at":null}}"#,
        )
        .expect("decode");
        let spend = snapshot_from_openrouter_key(capped).expect("maps").spend.expect("spend");
        assert_eq!(spend.limit, Some(20.0), "the cap is the denominator a bar needs");
        assert_eq!(spend.limit_remaining, Some(19.961_130_437), "what is left to spend");
        assert_eq!(spend.limit_reset.as_deref(), Some("monthly"), "the window the cap resets on");
        assert_eq!(spend.expires_at, None, "a key with no expiry reports none");

        let uncapped: forge_primitives::usage::openrouter::KeyResponse = serde_json::from_str(
            r#"{"data":{"usage_daily":0.56,"usage_weekly":1.25,"usage_monthly":20.30,
                        "limit":null,"limit_remaining":null,"limit_reset":null}}"#,
        )
        .expect("decode");
        let spend = snapshot_from_openrouter_key(uncapped).expect("maps").spend.expect("spend");
        assert_eq!(spend.limit, None, "an uncapped key has no denominator to invent");
        assert_eq!(spend.limit_remaining, None);
        assert_eq!(spend.limit_reset, None);
        assert!((spend.monthly - 20.30).abs() < f64::EPSILON, "spend still maps without a cap");
    }

    /// One figure present is a real report; its absent siblings are
    /// genuinely zero, which is what the endpoint means by omitting a
    /// figure that has a sibling.
    #[test]
    fn a_partial_openrouter_body_maps_its_present_figure() {
        let partial: forge_primitives::usage::openrouter::KeyResponse =
            serde_json::from_str(r#"{"data":{"usage_daily":0.25}}"#).expect("decode");
        let spend = snapshot_from_openrouter_key(partial)
            .expect("one present figure is a readable report")
            .spend
            .expect("spend");
        assert!((spend.daily - 0.25).abs() < f64::EPSILON, "the present figure maps");
        assert!(
            spend.weekly.abs() < f64::EPSILON && spend.monthly.abs() < f64::EPSILON,
            "absent siblings of a present figure are zero",
        );
    }
}
