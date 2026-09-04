//! Z.ai GLM coding plan monitor response shapes.
//!
//! Type-only - the HTTP fetcher lives in
//! `forge_agent::cloud::oauth_usage`. These are the JSON wire shapes;
//! the fetcher deserializes into them.
//!
//! Every Z.ai monitor endpoint answers HTTP 200 regardless of outcome
//! (a wrong key, a wrong path and a healthy response share the
//! status), so the verdict lives entirely inside the
//! `{code, msg, data, success}` envelope and callers key on
//! `success`/`code`, never on HTTP status.

use serde::{Deserialize, Serialize};

/// `/api/monitor/usage/quota/limit` envelope.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaLimitResponse {
    pub code: Option<i64>,
    pub msg: Option<String>,
    pub data: Option<QuotaLimitData>,
    #[serde(default)]
    pub success: bool,
}

/// The plan tier and its rolling credit windows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaLimitData {
    #[serde(default)]
    pub limits: Vec<QuotaLimitEntry>,
    /// Plan tier, e.g. `"max"` on GLM Coding Max.
    #[serde(default)]
    pub level: Option<String>,
}

/// One rolling window, in credits. `usage` is the per-window LIMIT
/// (28000 on a 5h Max window, 140000 weekly), not consumption;
/// consumption is `usage - remaining`. `unit` 3 counts hours (the
/// 5-hour window, `number` 5), unit 6 counts weeks (`number` 1).
///
/// Deliberately partial: the payload also carries a `percentage` and
/// a `currentValue` that lags at 0; neither is mapped.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaLimitEntry {
    /// Entry kind; only `"CREDIT_LIMIT"` entries carry the windows.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Time unit: 3 = hours, 6 = weeks.
    pub unit: Option<i64>,
    /// How many `unit`s the window spans.
    pub number: Option<i64>,
    /// The per-window credit limit.
    pub usage: Option<f64>,
    /// Credits still available in the window.
    pub remaining: Option<f64>,
    /// Epoch milliseconds. Absent on the 5-hour entry until the first
    /// successful request, which starts the window; the weekly entry
    /// resets on the purchase anniversary.
    pub next_reset_time: Option<i64>,
}

/// `/api/biz/subscription/list` envelope. `data` lists the account's
/// purchased plans.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionListResponse {
    pub code: Option<i64>,
    pub msg: Option<String>,
    pub data: Option<Vec<Subscription>>,
    #[serde(default)]
    pub success: bool,
}

/// One purchased plan. Deliberately partial: prices and the payment
/// channel are on the payload and unmapped.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    /// Purchased plan name, e.g. `"GLM Coding Max"`.
    pub product_name: Option<String>,
    /// e.g. `"VALID"`.
    pub status: Option<String>,
    /// e.g. `"monthly"`.
    pub billing_cycle: Option<String>,
    /// Renewal date as a bare date string - NOT epoch milliseconds
    /// like the quota endpoint's `nextResetTime`.
    pub next_renew_time: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh Max account: both windows at zero consumption, and the
    /// 5-hour entry carries no `nextResetTime` yet because no request
    /// has started its window. The unmapped `percentage` and
    /// `currentValue` fields are on the payload and must be tolerated.
    #[test]
    fn decodes_fresh_account_quota_limit() {
        let body = br#"{
            "code": 200,
            "msg": "success",
            "data": {
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,
                     "remaining":28000,"percentage":0,"currentValue":0},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":140000,"percentage":0,"currentValue":0,
                     "nextResetTime":1757000000000}
                ],
                "level": "max"
            },
            "success": true
        }"#;
        let envelope: QuotaLimitResponse = serde_json::from_slice(body).expect("decode");
        assert_eq!(envelope.code, Some(200));
        assert!(envelope.success);
        let data = envelope.data.expect("data");
        assert_eq!(data.level.as_deref(), Some("max"));
        assert_eq!(data.limits.len(), 2);
        let five_hour = &data.limits[0];
        assert_eq!(five_hour.kind.as_deref(), Some("CREDIT_LIMIT"));
        assert_eq!(five_hour.unit, Some(3));
        assert_eq!(five_hour.usage, Some(28000.0));
        assert_eq!(five_hour.remaining, Some(28000.0));
        assert_eq!(five_hour.next_reset_time, None, "5h entry has no reset until first request");
        let weekly = &data.limits[1];
        assert_eq!(weekly.unit, Some(6));
        assert_eq!(weekly.next_reset_time, Some(1_757_000_000_000));
    }

    /// After usage: consumption shows in `remaining`, and the 5-hour
    /// entry has grown a `nextResetTime` now that a request started
    /// its window.
    #[test]
    fn decodes_after_usage_quota_limit() {
        let body = br#"{
            "code": 200,
            "msg": "success",
            "data": {
                "limits": [
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,
                     "remaining":27104,"percentage":3.2,"currentValue":0,
                     "nextResetTime":1757025600000},
                    {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,
                     "remaining":139000,"percentage":0.71,"currentValue":0,
                     "nextResetTime":1757000000000}
                ],
                "level": "max"
            },
            "success": true
        }"#;
        let data =
            serde_json::from_slice::<QuotaLimitResponse>(body).expect("decode").data.expect("data");
        let five_hour = &data.limits[0];
        assert_eq!(five_hour.remaining, Some(27104.0));
        assert_eq!(five_hour.next_reset_time, Some(1_757_025_600_000));
    }

    /// Both auth negatives are body-level on an HTTP 200: the envelope
    /// must still decode so the failure can be keyed on
    /// `success`/`code` rather than on transport.
    #[test]
    fn decodes_failure_envelopes() {
        let wrong_key: QuotaLimitResponse = serde_json::from_slice(
            br#"{"code":401,"msg":"token expired or incorrect","success":false}"#,
        )
        .expect("decode");
        assert!(!wrong_key.success);
        assert_eq!(wrong_key.code, Some(401));
        assert_eq!(wrong_key.msg.as_deref(), Some("token expired or incorrect"));
        assert!(wrong_key.data.is_none());

        let wrong_path: QuotaLimitResponse =
            serde_json::from_slice(br#"{"code":500,"msg":"404 NOT_FOUND","success":false}"#)
                .expect("decode");
        assert!(!wrong_path.success);
        assert_eq!(wrong_path.code, Some(500));
    }

    /// `nextRenewTime` on the subscription list is a DATE STRING,
    /// unlike the quota endpoint's epoch milliseconds.
    #[test]
    fn decodes_subscription_list_with_date_string_renewal() {
        let body = br#"{
            "code": 200,
            "msg": "success",
            "data": [
                {"productName":"GLM Coding Max","status":"VALID",
                 "billingCycle":"monthly","nextRenewTime":"2026-10-04",
                 "paymentChannel":"STRIPE","autoRenew":true,
                 "prices":[{"amount":3000,"currency":"CNY"}]}
            ],
            "success": true
        }"#;
        let envelope: SubscriptionListResponse = serde_json::from_slice(body).expect("decode");
        let subs = envelope.data.expect("data");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].product_name.as_deref(), Some("GLM Coding Max"));
        assert_eq!(subs[0].status.as_deref(), Some("VALID"));
        assert_eq!(subs[0].billing_cycle.as_deref(), Some("monthly"));
        assert_eq!(subs[0].next_renew_time.as_deref(), Some("2026-10-04"));
    }
}
