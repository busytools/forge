//! Composable pieces the backends build probes and snapshots from:
//! the windowed `/api/oauth/usage` round-trip, the payload-to-window
//! mappers, and the timestamp parsing the keychain reader shares.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use forge_primitives::usage::oauth::{
    OauthExtraUsage, OauthUsage, OauthUsageError, OauthUsageWindow,
};
use forge_primitives::usage::{ExtraUsage, UsageSnapshot, UsageSourceKind, UsageWindow};

/// Per-call timeout the probes bake into their client.
pub const OAUTH_TIMEOUT: Duration = Duration::from_secs(8);

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub(crate) const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// The base-url credential an account's `env` carries: the
/// `ANTHROPIC_BASE_URL` endpoint and the `ANTHROPIC_AUTH_TOKEN` bearer
/// beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseUrlCredential {
    pub base_url: String,
    pub bearer: String,
}

/// The read of a base-url credential that found no usable
/// `ANTHROPIC_BASE_URL`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("no usable ANTHROPIC_BASE_URL in the account env for the base-url plan")]
pub struct MissingBase;

/// The base-url credential shared by every provider that authenticates
/// from `[accounts.env]`: `ANTHROPIC_BASE_URL` (trimmed, empty after
/// trim = missing) and `ANTHROPIC_AUTH_TOKEN`, absent = empty bearer -
/// a localhost proxy ignores the bearer and the probe must still fire.
pub fn base_url_credential<S: std::hash::BuildHasher>(
    env: &HashMap<String, String, S>,
) -> Result<BaseUrlCredential, MissingBase> {
    let Some(base_url) =
        env.get("ANTHROPIC_BASE_URL").map(|value| value.trim()).filter(|value| !value.is_empty())
    else {
        // Config load rejects a base-url provider with no
        // ANTHROPIC_BASE_URL (WorkspaceError::AccountProviderNeedsBaseUrl),
        // so this arm is unreachable in production; the tests below pin
        // it instead of a debug_assert, which would fire under them.
        return Err(MissingBase);
    };
    let bearer = env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str).unwrap_or_default();
    Ok(BaseUrlCredential { base_url: base_url.to_owned(), bearer: bearer.to_owned() })
}

/// Parse a JSON value (string OR number) as a `SystemTime`. Accepts:
/// - ISO-8601 datetime strings (e.g. `"2025-12-25T12:00:00.000Z"`).
/// - Integer-string seconds-or-milliseconds since UNIX epoch.
/// - Number seconds-or-milliseconds since UNIX epoch.
pub fn parse_timestamp_value(value: &Value) -> Option<SystemTime> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|raw| i64::try_from(raw).ok()))
            .and_then(system_time_from_epoch),
        Value::String(raw) => parse_iso8601_timestamp(raw)
            .or_else(|| raw.trim().parse::<i64>().ok().and_then(system_time_from_epoch)),
        _ => None,
    }
}

/// Convert a non-negative epoch integer to `SystemTime`. Values
/// `>= 1e12` are interpreted as milliseconds; smaller values as
/// seconds. Negative values fail.
pub fn system_time_from_epoch(raw: i64) -> Option<SystemTime> {
    if raw < 0 {
        return None;
    }
    let raw = u64::try_from(raw).ok()?;
    if raw >= 1_000_000_000_000 {
        Some(SystemTime::UNIX_EPOCH + Duration::from_millis(raw))
    } else {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(raw))
    }
}

/// Hand-rolled ISO-8601 / RFC-3339 datetime parser. Subset:
/// `YYYY-MM-DD[T| ]HH:MM[:SS[.fffffffff]][Z|+HH:MM|-HH:MM]`. Returns
/// `None` on any field-level parse failure.
fn parse_iso8601_timestamp(raw: &str) -> Option<SystemTime> {
    let trimmed = raw.trim();
    let (date_part, time_part) = trimmed.split_once('T').or_else(|| trimmed.split_once(' '))?;

    let mut date_iter = date_part.split('-');
    let year = date_iter.next()?.parse::<i32>().ok()?;
    let month = date_iter.next()?.parse::<u32>().ok()?;
    let day = date_iter.next()?.parse::<u32>().ok()?;

    let (time_only, offset_seconds) = split_time_and_offset(time_part)?;
    let mut time_iter = time_only.split(':');
    let hour = time_iter.next()?.parse::<u32>().ok()?;
    let minute = time_iter.next()?.parse::<u32>().ok()?;
    let second_and_fraction = time_iter.next().unwrap_or("0");
    let (second_raw, fraction_raw) =
        second_and_fraction.split_once('.').unwrap_or((second_and_fraction, ""));
    let second = second_raw.parse::<u32>().ok()?;

    let mut nanos = 0u32;
    let mut factor = 100_000_000u32;
    for ch in fraction_raw.chars().take(9) {
        let digit = ch.to_digit(10)?;
        nanos = nanos.saturating_add(digit.saturating_mul(factor));
        if factor == 0 {
            break;
        }
        factor /= 10;
    }

    let days = days_from_civil(year, month, day)?;
    let day_seconds =
        i64::from(hour) * 60 * 60 + i64::from(minute) * 60 + i64::from(second) - offset_seconds;
    let unix_seconds = days.checked_mul(86_400)?.checked_add(day_seconds)?;
    if unix_seconds < 0 {
        return None;
    }
    let secs = u64::try_from(unix_seconds).ok()?;
    Some(
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_nanos(u64::from(nanos)),
    )
}

fn split_time_and_offset(raw: &str) -> Option<(&str, i64)> {
    if let Some(rest) = raw.strip_suffix('Z') {
        return Some((rest, 0));
    }
    let split_idx = raw.rfind(['+', '-'])?;
    let (time_only, offset_str) = raw.split_at(split_idx);
    let sign: i64 = if offset_str.starts_with('+') { 1 } else { -1 };
    let offset_str = &offset_str[1..];
    let (h, m) = offset_str.split_once(':').unwrap_or((offset_str, "0"));
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    Some((time_only, sign * (h * 3600 + m * 60)))
}

/// Days since 1970-01-01 (Howard Hinnant's civil-from-days inverse).
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut year = i64::from(year);
    let month = i64::from(month);
    let day = i64::from(day);
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + day_of_year;
    Some(era * 146_097 + doe - 719_468)
}

/// The `/api/oauth/usage` endpoint URL. Defaults to the hardcoded
/// Anthropic host; a `base_url` override (an account's
/// `ANTHROPIC_BASE_URL`) redirects the probe to an alternate endpoint
/// serving the same `OauthUsage` shape. Any trailing slash on the
/// override is trimmed so `http://host/` and `http://host` behave
/// identically.
fn usage_url(base_url: Option<&str>) -> String {
    match base_url {
        Some(base) => format!("{}/api/oauth/usage", base.trim_end_matches('/')),
        None => OAUTH_USAGE_URL.to_owned(),
    }
}

/// One round-trip against `/api/oauth/usage` with the windowed
/// request shape: ACCEPT + content-type + the oauth-beta header +
/// `claude-code/<version>` UA + Bearer. Status classification:
/// 200 / 401 / 403-with-`oauth_scope_insufficient`-body / 429 +
/// Retry-After / other. The client and UA arrive injected; building
/// them is the host port's job.
pub async fn anthropic_windowed_probe(
    client: &reqwest::Client,
    user_agent: &str,
    base_url: Option<&str>,
    access_token: &str,
) -> Result<OauthUsage, OauthUsageError> {
    let response = client
        .get(usage_url(base_url))
        .headers(oauth_headers(user_agent, access_token)?)
        .send()
        .await
        .map_err(|error| OauthUsageError::Network(error.to_string()))?;

    let status = response.status().as_u16();
    // Parse Retry-After BEFORE consuming the response body - once
    // we call .bytes() the response object is moved. Anthropic
    // returns 429 with a per-account hold-down value in seconds;
    // honouring it prevents the poller from re-tripping the limit
    // every cycle.
    let retry_after = if status == 429 {
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after)
    } else {
        None
    };
    let body = response
        .bytes()
        .await
        .map_err(|error| OauthUsageError::Network(format!("body read: {error}")))?;

    // Diagnostic tracing for the "Anthropic 429s us on the first
    // probe" suspicion. Log status + a body suffix for every
    // non-200 response so a triage can correlate "which account /
    // when / what did the API actually say." Successful 200s are
    // logged at trace level (high volume - 60 s poll x N accounts)
    // with no body.
    if status == 200 {
        tracing::trace!(
            target: "forge_providers::anthropic",
            event_name = "oauth_usage_response",
            status,
            outcome = "ok",
            body_bytes = body.len(),
        );
    } else if status == 403 && is_scope_refusal(&body) {
        // The verdict on a valid setup token, not a failure: warn here
        // would fire every 60 s per healthy token account.
        tracing::debug!(
            target: "forge_providers::anthropic",
            event_name = "oauth_usage_scope_refusal",
            status,
            outcome = "scope_refused",
            body_suffix = %truncated_body_suffix(&body),
        );
    } else {
        tracing::warn!(
            target: "forge_providers::anthropic",
            event_name = "oauth_usage_response",
            status,
            outcome = "non_ok",
            retry_after_secs = ?retry_after.map(|d| d.as_secs()),
            body_suffix = %truncated_body_suffix(&body),
        );
    }

    match status {
        200 => serde_json::from_slice::<OauthUsage>(&body)
            .map_err(|error| OauthUsageError::Decode(error.to_string())),
        403 if is_scope_refusal(&body) => Err(OauthUsageError::ScopeInsufficient),
        401 | 403 => Err(OauthUsageError::Unauthorized(status)),
        429 => Err(OauthUsageError::RateLimited { retry_after }),
        _ => Err(OauthUsageError::HttpStatus(status, truncated_body_suffix(&body))),
    }
}

fn oauth_headers(user_agent: &str, access_token: &str) -> Result<HeaderMap, OauthUsageError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("anthropic-beta", HeaderValue::from_static(OAUTH_BETA_HEADER));
    let ua = HeaderValue::from_str(user_agent)
        .map_err(|error| OauthUsageError::Network(format!("bad UA header: {error}")))?;
    headers.insert(USER_AGENT, ua);
    let bearer = HeaderValue::from_str(&format!("Bearer {access_token}"))
        .map_err(|error| OauthUsageError::Network(format!("bad bearer header: {error}")))?;
    headers.insert(AUTHORIZATION, bearer);
    Ok(headers)
}

/// Whether a 403 body is the usage endpoint's scope refusal rather than
/// an auth failure. Keyed on the body's `error.details.error_code` -
/// the verified live shape for a valid setup token; a revoked one
/// answers 401 `authentication_error`, so the two never share a class.
fn is_scope_refusal(body: &[u8]) -> bool {
    let code = serde_json::from_slice::<Value>(body).ok().and_then(|value| {
        value.get("error")?.get("details")?.get("error_code")?.as_str().map(str::to_owned)
    });
    code.as_deref() == Some("oauth_scope_insufficient")
}

/// RFC 7231 section 7.1.3 `Retry-After` accepts either delta-seconds
/// (`"120"`) or an HTTP-date (`"Wed, 21 Oct 2015 07:28:00 GMT"`).
/// Anthropic emits the integer form today, but the spec leaves the
/// HTTP-date form open and proxies / CDNs in the path may swap shapes.
/// Try the integer form first; fall back to httpdate parsing and
/// compute the delta from `now`.
pub fn parse_retry_after(raw: &str) -> Option<Duration> {
    let trimmed = raw.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = httpdate::parse_http_date(trimmed).ok()?;
    when.duration_since(SystemTime::now()).ok()
}

/// First 200 chars of a body, as a `": ..."` suffix for log fields.
pub fn truncated_body_suffix(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body).trim().replace('\n', " ");
    if text.is_empty() {
        return String::new();
    }
    let shortened = if text.chars().count() > 200 {
        let mut out = text.chars().take(200).collect::<String>();
        out.push_str("...");
        out
    } else {
        text
    };
    format!(": {shortened}")
}

/// Map a windowed payload to a snapshot with every window treated as
/// independently optional, never requiring `five_hour`. This is the
/// base-url and token path's mapper: an alternate-endpoint proxy emits
/// each window on its own (`{}`, `{five_hour}`, `{seven_day}`,
/// `{five_hour, seven_day}`), and a missing `five_hour` is a valid
/// steady state - the cold start before the first upstream request,
/// and the post-5h-reset window where the proxy drops the key
/// entirely - not a malformed response. `{}` maps to all-None (n/a
/// bars); an out-of-contract error-shaped 200 lands here as n/a too
/// rather than erroring. Infallible - there is no window this can
/// reject.
pub fn snapshot_from_payload_lenient(payload: OauthUsage) -> UsageSnapshot {
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

pub(crate) fn map_window(payload: Option<OauthUsageWindow>) -> Option<UsageWindow> {
    let payload = payload?;
    let utilization = payload.utilization?;
    Some(UsageWindow {
        utilization: utilization.clamp(0.0, 100.0),
        resets_at: payload.resets_at.as_ref().and_then(parse_timestamp_value),
        reset_description: None,
    })
}

pub(crate) fn map_extra_usage(payload: Option<OauthExtraUsage>) -> Option<ExtraUsage> {
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
    fn usage_url_defaults_to_anthropic_host() {
        assert_eq!(usage_url(None), OAUTH_USAGE_URL);
    }

    #[test]
    fn usage_url_uses_base_url_override_and_trims_trailing_slash() {
        assert_eq!(
            usage_url(Some("http://localhost:18765")),
            "http://localhost:18765/api/oauth/usage",
        );
        assert_eq!(
            usage_url(Some("http://localhost:18765/")),
            "http://localhost:18765/api/oauth/usage",
            "trailing slash trimmed so host and host/ behave identically",
        );
    }

    #[test]
    fn base_url_credential_reads_the_base_and_bearer_pair() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-codex".to_owned());
        // A stray setup token must not leak into the base-url pair: the
        // base-url providers authenticate with ANTHROPIC_AUTH_TOKEN only.
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "setup-token".to_owned());
        assert_eq!(
            base_url_credential(&env),
            Ok(BaseUrlCredential {
                base_url: "http://localhost:18765".to_owned(),
                bearer: "sk-codex".to_owned(),
            }),
        );
    }

    #[test]
    fn base_url_credential_trims_whitespace_around_the_base() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "  http://localhost:18765  ".to_owned());
        let credential = base_url_credential(&env).expect("credential");
        assert_eq!(credential.base_url, "http://localhost:18765");
    }

    /// An absent or blank ANTHROPIC_AUTH_TOKEN must not suppress the
    /// probe: an empty bearer goes out and a localhost proxy ignores it.
    #[test]
    fn base_url_credential_defaults_the_bearer_to_empty() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        assert_eq!(base_url_credential(&env).expect("credential").bearer, "");
        assert_eq!(base_url_credential(&HashMap::new()).expect_err("no base"), MissingBase);
    }

    #[test]
    fn base_url_credential_rejects_a_base_that_is_empty_after_trim() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "   ".to_owned());
        assert_eq!(base_url_credential(&env), Err(MissingBase));
    }

    /// A down endpoint must surface as the Network class and the probe
    /// must return rather than hang: preflight's bounded-failure path
    /// leans on both. Port 1 on loopback refuses the connect at once.
    #[tokio::test]
    async fn a_down_endpoint_is_a_network_failure_and_the_probe_returns() {
        let client = reqwest::Client::builder().build().expect("client");
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            anthropic_windowed_probe(&client, "claude-code/1.0.0", Some("http://127.0.0.1:1"), "t"),
        )
        .await
        .expect("the probe returns against an unreachable endpoint");
        assert!(
            matches!(result, Err(OauthUsageError::Network(_))),
            "a refused connect is the Network class, not a status or decode; got {result:?}"
        );
    }

    #[test]
    fn retry_after_integer_seconds_round_trip() {
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
        assert_eq!(parse_retry_after("  120  "), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("3600"), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn retry_after_http_date_returns_delta_from_now() {
        // ~1 hour in the future, formatted in HTTP-date format
        let target = SystemTime::now() + Duration::from_secs(3600);
        let formatted = httpdate::fmt_http_date(target);
        let parsed = parse_retry_after(&formatted).expect("http-date parses");
        // The parsed delta should be close to 1 hour (allow +-5 s drift).
        assert!(parsed.as_secs() >= 3595 && parsed.as_secs() <= 3605, "got {parsed:?}");
    }

    #[test]
    fn retry_after_past_http_date_returns_none() {
        // HTTP-date in the past - duration_since(now) returns Err -> None.
        let past = SystemTime::now() - Duration::from_secs(3600);
        let formatted = httpdate::fmt_http_date(past);
        assert!(parse_retry_after(&formatted).is_none());
    }

    #[test]
    fn retry_after_garbage_returns_none() {
        assert!(parse_retry_after("not a duration").is_none());
        assert!(parse_retry_after("").is_none());
    }

    #[test]
    fn decodes_sparse_oauth_payload() {
        let usage: OauthUsage = serde_json::from_slice(
            br#"{
                "five_hour": { "utilization": 12.5, "resets_at": "2025-12-25T12:00:00.000Z" },
                "seven_day_sonnet": { "utilization": 5 },
                "unknown_field": true
            }"#,
        )
        .expect("decode");
        assert_eq!(usage.five_hour.as_ref().and_then(|w| w.utilization), Some(12.5));
        assert_eq!(usage.seven_day_sonnet.as_ref().and_then(|w| w.utilization), Some(5.0));
        assert!(usage.seven_day.is_none());
    }

    #[test]
    fn decodes_extra_usage_in_minor_units() {
        let usage: OauthUsage = serde_json::from_slice(
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
        let extra = usage.extra_usage.expect("extra usage");
        assert_eq!(extra.monthly_limit, Some(2000.0));
        assert_eq!(extra.used_credits, Some(1240.0));
        assert_eq!(extra.utilization, Some(62.0));
        assert_eq!(extra.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn lenient_maps_seven_day_only_without_erroring() {
        // Post-5h-reset steady state: the proxy drops the `five_hour`
        // key entirely (serde skip) and sends only `seven_day`. That
        // must map to a snapshot with five_hour None + seven_day Some,
        // NOT a fetch error - the earlier all-absent-else-strict logic
        // routed this to the strict mapper and flipped the account to a
        // fetch error every 5h cycle.
        let payload: OauthUsage =
            serde_json::from_slice(br#"{"seven_day":{"utilization":10.0}}"#).expect("decode");
        let snapshot = snapshot_from_payload_lenient(payload);
        assert!(snapshot.five_hour.is_none());
        assert_eq!(snapshot.seven_day.as_ref().map(|window| window.utilization), Some(10.0));
    }

    #[test]
    fn lenient_maps_empty_payload_to_all_none_snapshot() {
        // A base-url account's proxy returns `{}` until warm; that must
        // become an all-None snapshot (n/a bars), not a fetch error.
        let snapshot = snapshot_from_payload_lenient(OauthUsage::default());
        assert!(snapshot.five_hour.is_none());
        assert!(snapshot.seven_day.is_none());
        assert!(snapshot.seven_day_opus.is_none());
        assert!(snapshot.seven_day_sonnet.is_none());
        assert_eq!(snapshot.source, UsageSourceKind::Oauth);
    }

    #[test]
    fn lenient_maps_five_hour_only_populated() {
        let payload: OauthUsage = serde_json::from_slice(
            br#"{ "five_hour": { "utilization": 42.0, "resets_at": "2025-12-25T12:00:00.000Z" } }"#,
        )
        .expect("decode");
        let snapshot = snapshot_from_payload_lenient(payload);
        assert_eq!(snapshot.five_hour.as_ref().map(|window| window.utilization), Some(42.0));
        assert!(snapshot.seven_day.is_none());
    }

    #[test]
    fn maps_extra_usage_amounts_in_major_units() {
        let payload: OauthUsage = serde_json::from_slice(
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
        let extra = map_extra_usage(payload.extra_usage).expect("extra usage");
        assert_eq!(extra.monthly_limit, Some(20.0));
        assert_eq!(extra.used_credits, Some(12.4));
        assert_eq!(extra.utilization, Some(62.0));
        assert_eq!(extra.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn map_window_clamps_utilization_and_parses_resets_at() {
        let payload: OauthUsageWindow = serde_json::from_str(
            r#"{ "utilization": 140.0, "resets_at": "2025-12-25T12:00:00.000Z" }"#,
        )
        .expect("decode");
        let window = map_window(Some(payload)).expect("window");
        assert!(
            (window.utilization - 100.0).abs() < f64::EPSILON,
            "utilization clamps to the percentage range",
        );
        assert_eq!(
            window.resets_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_766_664_000)),
        );
    }

    /// The verified after-usage shape: a scope refusal classifies, an
    /// authentication_error does not, and neither does a non-JSON body
    /// or a populated-but-different error_code.
    #[test]
    fn a_403_scope_refusal_body_is_recognized_and_other_403s_are_not() {
        let refused = br#"{"type":"error","error":{"type":"permission_error","message":"OAuth token does not meet scope requirement user:profile","details":{"error_code":"oauth_scope_insufficient"}}}"#;
        assert!(
            is_scope_refusal(refused),
            "the verified refusal shape classifies as a scope refusal"
        );
        assert!(
            !is_scope_refusal(br#"{"type":"error","error":{"type":"authentication_error","message":"Invalid bearer token","details":{}}}"#),
            "an authentication_error body is not a scope refusal",
        );
        assert!(!is_scope_refusal(b"not json"), "a non-JSON body is not a scope refusal");
        assert!(
            !is_scope_refusal(br#"{"error":{"type":"permission_error","message":"no details"}}"#),
            "a permission_error without the error_code is not a scope refusal",
        );
        assert!(
            !is_scope_refusal(
                br#"{"error":{"details":{"error_code":"oauth_token_revoked"},"message":"x"}}"#
            ),
            "a populated but different error_code is not a scope refusal",
        );
    }

    #[test]
    fn parses_iso8601_timestamp() {
        let parsed = parse_iso8601_timestamp("2025-12-25T12:00:00.000Z").expect("timestamp");
        assert_eq!(
            parsed.duration_since(SystemTime::UNIX_EPOCH).expect("after epoch"),
            Duration::from_secs(1_766_664_000),
            "2025-12-25T12:00:00Z == epoch second 1766664000"
        );
    }

    #[test]
    fn parses_numeric_millisecond_timestamp() {
        let parsed =
            parse_timestamp_value(&serde_json::json!(1_735_128_000_000_i64)).expect("timestamp");
        assert_eq!(
            parsed.duration_since(SystemTime::UNIX_EPOCH).expect("after epoch"),
            Duration::from_secs(1_735_128_000),
            "milliseconds must land on the matching epoch second"
        );
    }

    /// A negative UTC offset applies its true sign: the same wall
    /// clock written in -05:30 lands 19_800s LATER than the Z form.
    #[test]
    fn parses_negative_offset_timestamp() {
        let parsed = parse_iso8601_timestamp("2025-12-25T06:30:00-05:30").expect("timestamp");
        assert_eq!(
            parsed.duration_since(SystemTime::UNIX_EPOCH).expect("after epoch"),
            Duration::from_secs(1_766_664_000),
            "06:30-05:30 is the same instant as 12:00Z"
        );
    }

    /// Pre-epoch instants are rejected on purpose: `SystemTime +
    /// Duration` cannot represent them, so a negative epoch returns
    /// None rather than silently wrapping or clamping.
    #[test]
    fn rejects_pre_epoch_timestamps() {
        assert!(parse_iso8601_timestamp("1969-12-31T23:59:59.000Z").is_none());
    }
}
