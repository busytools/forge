use std::time::SystemTime;

use crate::cloud::time::parse_timestamp_value;
use crate::cloud::{ExtraUsage, UsageSnapshot, UsageSourceKind, UsageWindow};

#[derive(Debug)]
pub enum OauthFetchError {
    Unavailable(String),
    Unauthorized(String),
    Failed(String),
}

impl OauthFetchError {
    pub fn should_fallback_to_cli(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Unauthorized(_))
    }

    pub fn into_message(self) -> String {
        match self {
            Self::Unavailable(message) | Self::Unauthorized(message) | Self::Failed(message) => {
                message
            }
        }
    }
}

impl From<super::oauth_usage::OauthUsageError> for OauthFetchError {
    fn from(error: super::oauth_usage::OauthUsageError) -> Self {
        use super::oauth_usage::OauthUsageError;
        match error {
            OauthUsageError::NoCredentials => Self::Unavailable(
                "No Claude OAuth credentials found. Run `claude auth login` in a terminal.".to_owned(),
            ),
            OauthUsageError::Expired => Self::Unavailable(
                "Claude OAuth credentials expired. Run `claude auth login` in a terminal to refresh them.".to_owned(),
            ),
            OauthUsageError::Unauthorized(_) => Self::Unauthorized(
                "Claude OAuth usage request was rejected. Run `claude auth login` in a terminal to refresh credentials."
                    .to_owned(),
            ),
            OauthUsageError::HttpStatus(status, suffix) => {
                Self::Failed(format!("Claude OAuth usage request failed with HTTP {status}{suffix}"))
            }
            OauthUsageError::Network(message) => {
                Self::Failed(format!("Claude OAuth network error: {message}"))
            }
            OauthUsageError::Decode(message) => {
                Self::Failed(format!("Failed to decode Claude OAuth usage response: {message}"))
            }
            // `OauthUsageError` is #[non_exhaustive]; route unknown
            // future variants through a generic failure so the match
            // stays exhaustive across crate boundaries.
            other => Self::Failed(format!("Claude OAuth error: {other}")),
        }
    }
}

pub async fn fetch_snapshot(conn: &crate::AgentHandle) -> Result<UsageSnapshot, OauthFetchError> {
    let payload = conn.oauth_usage().await?;
    map_usage_payload(payload)
}

fn map_usage_payload(
    payload: super::oauth_usage::OauthUsage,
) -> Result<UsageSnapshot, OauthFetchError> {
    let five_hour = map_window(payload.five_hour, "5-hour");
    if five_hour.is_none() {
        return Err(OauthFetchError::Failed(
            "Claude OAuth usage response did not include the current session window.".to_owned(),
        ));
    }
    Ok(UsageSnapshot {
        source: UsageSourceKind::Oauth,
        fetched_at: SystemTime::now(),
        five_hour,
        seven_day: map_window(payload.seven_day, "7-day"),
        seven_day_opus: map_window(payload.seven_day_opus, "7-day Opus"),
        seven_day_sonnet: map_window(payload.seven_day_sonnet, "7-day Sonnet"),
        extra_usage: map_extra_usage(payload.extra_usage),
    })
}

fn map_window(
    payload: Option<super::oauth_usage::OauthUsageWindow>,
    label: &'static str,
) -> Option<UsageWindow> {
    let payload = payload?;
    let utilization = payload.utilization?;
    Some(UsageWindow {
        label,
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
    fn maps_sparse_oauth_payload() {
        let payload: crate::cloud::oauth_usage::OauthUsage = serde_json::from_slice(
            br#"{
                "five_hour": { "utilization": 12.5, "resets_at": "2025-12-25T12:00:00.000Z" },
                "seven_day_sonnet": { "utilization": 5 },
                "unknown_field": true
            }"#,
        )
        .expect("decode");
        let snapshot = map_usage_payload(payload).expect("snapshot");
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
        let snapshot = map_usage_payload(payload).expect("snapshot");
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
