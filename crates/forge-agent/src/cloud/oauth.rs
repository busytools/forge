use std::time::SystemTime;

use crate::cloud::time::parse_timestamp_value;
use crate::cloud::{ExtraUsage, UsageSnapshot, UsageSourceKind, UsageWindow};

/// Why a probe payload could not be mapped to a [`UsageSnapshot`].
/// Only [`snapshot_from_payload`] produces this (a keychain 200 that
/// omits the session window); the base-url path maps leniently and
/// never fails. Callers log it and back off - they don't branch on a
/// cause - so a single message-carrying variant is all that's needed.
#[derive(Debug)]
pub enum OauthFetchError {
    Failed(String),
}

/// Map a fetched [`OauthUsage`](super::oauth_usage::OauthUsage)
/// payload into the TUI-facing [`UsageSnapshot`]. Exposed so the
/// workspace facade can pump the payload through this same mapping
/// without exposing `AgentHandle` to its caller.
pub fn snapshot_from_payload(
    payload: super::oauth_usage::OauthUsage,
) -> Result<UsageSnapshot, OauthFetchError> {
    let five_hour = map_window(payload.five_hour);
    if five_hour.is_none() {
        return Err(OauthFetchError::Failed(
            "Claude OAuth usage response did not include the current session window.".to_owned(),
        ));
    }
    Ok(UsageSnapshot {
        source: UsageSourceKind::Oauth,
        fetched_at: SystemTime::now(),
        five_hour,
        seven_day: map_window(payload.seven_day),
        seven_day_opus: map_window(payload.seven_day_opus),
        seven_day_sonnet: map_window(payload.seven_day_sonnet),
        extra_usage: map_extra_usage(payload.extra_usage),
        spend: None,
    })
}

/// Maps a payload to a snapshot with every window treated as
/// independently optional, never requiring `five_hour`. This is the
/// base-url path's mapper: an alternate-endpoint proxy emits each
/// window on its own (`{}`, `{five_hour}`, `{seven_day}`,
/// `{five_hour, seven_day}`), and a missing `five_hour` is a valid
/// steady state - the cold start before the first upstream request,
/// and the post-5h-reset window where the proxy drops the key
/// entirely - not a malformed response. `{}` maps to all-None (n/a
/// bars); an out-of-contract error-shaped 200 lands here as n/a too
/// rather than erroring. Contrast [`snapshot_from_payload`], which
/// requires `five_hour` and guards the default Anthropic path where a
/// 200 without it signals response-shape drift. Infallible - there is
/// no window this can reject.
pub fn snapshot_from_payload_lenient(payload: super::oauth_usage::OauthUsage) -> UsageSnapshot {
    UsageSnapshot {
        source: UsageSourceKind::Oauth,
        fetched_at: SystemTime::now(),
        five_hour: map_window(payload.five_hour),
        seven_day: map_window(payload.seven_day),
        seven_day_opus: map_window(payload.seven_day_opus),
        seven_day_sonnet: map_window(payload.seven_day_sonnet),
        extra_usage: map_extra_usage(payload.extra_usage),
        spend: None,
    }
}

/// Map a probe payload to a snapshot, picking the mapper the probe
/// plan calls for: a base-url account maps leniently (each window
/// independent, never erroring), a keychain account maps strictly (a
/// 200 must carry the five-hour window). The loader and poller share
/// this so their base-url-vs-keychain mapping never drifts apart.
pub fn map_probe_snapshot(
    is_base_url: bool,
    payload: super::oauth_usage::OauthUsage,
) -> Result<UsageSnapshot, OauthFetchError> {
    if is_base_url {
        Ok(snapshot_from_payload_lenient(payload))
    } else {
        snapshot_from_payload(payload)
    }
}

/// Map an OpenRouter `/api/v1/key` payload into a [`UsageSnapshot`].
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
        spend: Some(forge_primitives::usage::ApiSpend {
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

fn map_window(payload: Option<super::oauth_usage::OauthUsageWindow>) -> Option<UsageWindow> {
    let payload = payload?;
    let utilization = payload.utilization?;
    Some(UsageWindow {
        utilization: utilization.clamp(0.0, 100.0),
        resets_at: payload.resets_at.as_ref().and_then(parse_timestamp_value),
        reset_description: None,
    })
}

fn map_extra_usage(payload: Option<super::oauth_usage::OauthExtraUsage>) -> Option<ExtraUsage> {
    let payload = payload?;
    if payload.is_enabled == Some(false) {
        return None;
    }
    Some(ExtraUsage {
        monthly_limit: payload.monthly_limit.map(|value| value / 100.0),
        used_credits: payload.used_credits.map(|value| value / 100.0),
        utilization: payload.utilization.map(|value| value.clamp(0.0, 100.0)),
        currency: payload.currency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_probe_snapshot_routes_by_plan() {
        // The seven-day-only shape (post-5h-reset steady state) is valid
        // on the base-url path but not the keychain path.
        let seven_day_only = || -> crate::cloud::oauth_usage::OauthUsage {
            serde_json::from_slice(br#"{"seven_day":{"utilization":10.0}}"#).expect("decode")
        };
        let lenient = map_probe_snapshot(true, seven_day_only()).expect("base-url maps leniently");
        assert!(lenient.five_hour.is_none());
        assert_eq!(lenient.seven_day.as_ref().map(|window| window.utilization), Some(10.0));
        assert!(
            map_probe_snapshot(false, seven_day_only()).is_err(),
            "keychain path still requires five_hour",
        );
    }

    #[test]
    fn lenient_maps_seven_day_only_without_erroring() {
        // Post-5h-reset steady state: the proxy drops the `five_hour`
        // key entirely (serde skip) and sends only `seven_day`. That
        // must map to a snapshot with five_hour None + seven_day Some,
        // NOT a fetch error - the earlier all-absent-else-strict logic
        // routed this to the strict mapper and flipped the account to a
        // fetch error every 5h cycle.
        let payload: crate::cloud::oauth_usage::OauthUsage =
            serde_json::from_slice(br#"{"seven_day":{"utilization":10.0}}"#).expect("decode");
        let snapshot = snapshot_from_payload_lenient(payload);
        assert!(snapshot.five_hour.is_none());
        assert_eq!(snapshot.seven_day.as_ref().map(|window| window.utilization), Some(10.0));
    }

    #[test]
    fn lenient_maps_empty_payload_to_all_none_snapshot() {
        // A base-url account's proxy returns `{}` until warm; that must
        // become an all-None snapshot (n/a bars), not a fetch error.
        let snapshot =
            snapshot_from_payload_lenient(crate::cloud::oauth_usage::OauthUsage::default());
        assert!(snapshot.five_hour.is_none());
        assert!(snapshot.seven_day.is_none());
        assert!(snapshot.seven_day_opus.is_none());
        assert!(snapshot.seven_day_sonnet.is_none());
        assert_eq!(snapshot.source, UsageSourceKind::Oauth);
    }

    #[test]
    fn lenient_maps_five_hour_only_populated() {
        let payload: crate::cloud::oauth_usage::OauthUsage = serde_json::from_slice(
            br#"{ "five_hour": { "utilization": 42.0, "resets_at": "2025-12-25T12:00:00.000Z" } }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_payload_lenient(payload);
        assert_eq!(snapshot.five_hour.as_ref().map(|window| window.utilization), Some(42.0));
        assert!(snapshot.seven_day.is_none());
    }

    #[test]
    fn maps_sparse_oauth_payload() {
        let payload: crate::cloud::oauth_usage::OauthUsage = serde_json::from_slice(
            br#"{
                "five_hour": { "utilization": 12.5, "resets_at": "2025-12-25T12:00:00.000Z" },
                "seven_day_sonnet": { "utilization": 5 },
                "unknown_field": true
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_payload(payload).expect("snapshot");
        assert_eq!(snapshot.five_hour.as_ref().map(|window| window.utilization), Some(12.5));
        assert_eq!(snapshot.seven_day_sonnet.as_ref().map(|window| window.utilization), Some(5.0));
        assert!(snapshot.seven_day.is_none());
    }

    #[test]
    fn maps_extra_usage_amounts_in_major_units() {
        let payload: crate::cloud::oauth_usage::OauthUsage = serde_json::from_slice(
            br#"{
                "five_hour": { "utilization": 1, "resets_at": "2025-12-25T12:00:00.000Z" },
                "extra_usage": {
                    "is_enabled": true,
                    "monthly_limit": 2000,
                    "used_credits": 1240,
                    "utilization": 62,
                    "currency": "USD"
                }
            }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_payload(payload).expect("snapshot");
        let extra = snapshot.extra_usage.expect("extra usage");
        assert_eq!(extra.monthly_limit, Some(20.0));
        assert_eq!(extra.used_credits, Some(12.4));
        assert_eq!(extra.utilization, Some(62.0));
        assert_eq!(extra.currency.as_deref(), Some("USD"));
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

    #[test]
    fn parses_iso8601_timestamp() {
        use crate::cloud::time::parse_iso8601_timestamp;
        use std::time::Duration;
        let parsed = parse_iso8601_timestamp("2025-12-25T12:00:00.000Z").expect("timestamp");
        // 2025-12-25T12:00:00Z is a fixed epoch second; pinning it
        // catches both a wrong parse and a silently dropped subsecond.
        assert_eq!(
            parsed.duration_since(std::time::UNIX_EPOCH).expect("after epoch"),
            Duration::from_secs(1_766_664_000),
            "2025-12-25T12:00:00Z == epoch second 1766664000"
        );
    }

    #[test]
    fn parses_numeric_millisecond_timestamp() {
        let parsed =
            parse_timestamp_value(&serde_json::json!(1_735_128_000_000_i64)).expect("timestamp");
        assert_eq!(
            parsed.duration_since(std::time::UNIX_EPOCH).expect("after epoch"),
            std::time::Duration::from_secs(1_735_128_000),
            "milliseconds must land on the matching epoch second"
        );
    }

    /// A negative UTC offset applies its true sign: the same wall
    /// clock written in -05:30 lands 19_800s LATER than the Z form.
    #[test]
    fn parses_negative_offset_timestamp() {
        use crate::cloud::time::parse_iso8601_timestamp;
        use std::time::Duration;
        let parsed = parse_iso8601_timestamp("2025-12-25T06:30:00-05:30").expect("timestamp");
        assert_eq!(
            parsed.duration_since(std::time::UNIX_EPOCH).expect("after epoch"),
            Duration::from_secs(1_766_664_000),
            "06:30-05:30 is the same instant as 12:00Z"
        );
    }

    /// Pre-epoch instants are rejected on purpose: `SystemTime +
    /// Duration` cannot represent them, so a negative epoch returns
    /// None rather than silently wrapping or clamping.
    #[test]
    fn rejects_pre_epoch_timestamps() {
        use crate::cloud::time::parse_iso8601_timestamp;
        assert!(parse_iso8601_timestamp("1969-12-31T23:59:59.000Z").is_none());
    }
}
