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

    #[test]
    fn parses_iso8601_timestamp() {
        use crate::cloud::time::parse_iso8601_timestamp;
        use std::time::UNIX_EPOCH;
        let parsed = parse_iso8601_timestamp("2025-12-25T12:00:00.000Z").expect("timestamp");
        assert!(parsed > UNIX_EPOCH);
    }

    #[test]
    fn parses_numeric_millisecond_timestamp() {
        use std::time::UNIX_EPOCH;
        let parsed =
            parse_timestamp_value(&serde_json::json!(1_735_128_000_000_i64)).expect("timestamp");
        assert!(parsed > UNIX_EPOCH);
    }
}
