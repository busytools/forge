//! The Slack Web API client: one uniform POST per method, form params,
//! Bearer token, and the envelope rules that turn a logical failure into
//! an error.
//!
//! Slack answers `{"ok": false, "error": "..."}` with HTTP 200 for a
//! logical failure, so a 200 is not success. Only a 429 arrives as a
//! status, and it carries `Retry-After`.

use std::time::Duration;

use forge_primitives::slack::SlackConversation;
use serde::Deserialize;
use serde::de::DeserializeOwned;

const API_ROOT: &str = "https://slack.com/api";

/// Used when the response carries no `Retry-After`.
pub(crate) const RETRY_FALLBACK: Duration = Duration::from_secs(5);
/// Floor for a `Retry-After` that asks for no wait at all. `Retry-After: 0`
/// is legal, and retrying at once is the opposite of what it asked.
pub(crate) const RETRY_FLOOR: Duration = Duration::from_secs(1);
/// Ceiling for a hostile or broken `Retry-After`.
pub(crate) const RETRY_CAP: Duration = Duration::from_secs(60);

/// What to wait after a 429. Slack sends whole seconds.
pub(crate) fn retry_delay(retry_after_secs: Option<u64>) -> Duration {
    match retry_after_secs {
        None => RETRY_FALLBACK,
        Some(secs) => Duration::from_secs(secs).clamp(RETRY_FLOOR, RETRY_CAP),
    }
}

/// Bounded so a gateway's error page cannot flood the log or the tool output.
const BODY_PREFIX_LEN: usize = 200;

fn body_prefix(body: &str) -> String {
    body.chars().take(BODY_PREFIX_LEN).collect()
}

/// A non-success status other than 429. The body may be an HTML error
/// page, which the envelope decoder would report as a JSON parse failure
/// - the status is what the operator needs, so name it and bound the body.
fn status_failure(method: &str, status: reqwest::StatusCode, body: &str) -> SlackError {
    SlackError::Transport {
        method: method.to_owned(),
        detail: format!("HTTP {status}: {}", body_prefix(body)),
    }
}

/// `application/x-www-form-urlencoded` body for one call.
fn encode_form(params: &[(&str, String)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

/// `Debug` is hand-written because the token must never be printed.
pub struct SlackClient {
    http: reqwest::Client,
    token: String,
}

impl std::fmt::Debug for SlackClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackClient").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum SlackError {
    /// Slack answered `ok: false`. `needed` names the scope when it is a scope failure.
    Api { method: String, error: String, needed: Option<String> },
    /// The transport failed, or the body was not the envelope we expect.
    Transport { method: String, detail: String },
    /// HTTP 429. Slack's Web API surfaces these as a status, not an `ok: false`.
    RateLimited { method: String, retry_after: Duration },
}

impl std::fmt::Display for SlackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api { method, error, needed } => {
                write!(f, "slack {method} failed: {error}")?;
                if let Some(needed) = needed {
                    write!(f, " (needs {needed})")?;
                }
                Ok(())
            }
            Self::Transport { method, detail } => write!(f, "slack {method} transport: {detail}"),
            Self::RateLimited { method, retry_after } => {
                write!(f, "slack {method} rate limited, retry in {}s", retry_after.as_secs())
            }
        }
    }
}

impl std::error::Error for SlackError {}

/// Slack wraps every payload in `ok`, and reports logical failures with HTTP 200.
#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    needed: Option<String>,
    #[serde(flatten)]
    value: T,
}

/// Split out of `call` so the envelope rules are testable without HTTP.
pub(crate) fn decode_envelope<T: DeserializeOwned>(
    method: &str,
    body: &str,
) -> Result<T, SlackError> {
    let envelope: Envelope<T> = serde_json::from_str(body).map_err(|detail| {
        SlackError::Transport { method: method.to_owned(), detail: detail.to_string() }
    })?;
    if !envelope.ok {
        return Err(SlackError::Api {
            method: method.to_owned(),
            error: envelope.error.unwrap_or_else(|| "unknown".to_owned()),
            needed: envelope.needed,
        });
    }
    Ok(envelope.value)
}

/// `auth.test` - who the token belongs to.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthTest {
    pub team: String,
    pub user: String,
    pub team_id: String,
    pub user_id: String,
    pub url: String,
}

/// `types` must be passed explicitly or Slack returns public channels
/// only, and `limit` must be passed or the call gets a stricter cap.
const CONVERSATION_TYPES: &str = "public_channel,private_channel,im,mpim";

/// A cursor Slack hands back unchanged must not spin the walk forever:
/// at 200 per page this is far past any real workspace, and the failure
/// mode is an endless request loop against a rate-limited API.
const MAX_CONVERSATION_PAGES: usize = 200;

/// One page of `users.conversations` as Slack sends it.
#[derive(Debug, Deserialize)]
struct RawConversationsPage {
    channels: Vec<SlackConversation>,
    #[serde(default)]
    response_metadata: Metadata,
}

#[derive(Debug, Default, Deserialize)]
struct Metadata {
    #[serde(default)]
    next_cursor: String,
}

/// One page with the cursor normalised: Slack sends an empty string on
/// the last page, which would otherwise read as a cursor to page from.
#[derive(Debug)]
struct ConversationsPage {
    conversations: Vec<SlackConversation>,
    next_cursor: Option<String>,
}

/// Split out of the async path so the paging rules are testable without HTTP.
fn decode_conversations_page(body: &str) -> Result<ConversationsPage, SlackError> {
    let raw: RawConversationsPage = decode_envelope("users.conversations", body)?;
    Ok(page_from_raw(raw))
}

fn page_from_raw(raw: RawConversationsPage) -> ConversationsPage {
    let cursor = (!raw.response_metadata.next_cursor.is_empty())
        .then_some(raw.response_metadata.next_cursor);
    ConversationsPage { conversations: raw.channels, next_cursor: cursor }
}

impl SlackClient {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }

    /// POST one Web API method. Slack is uniform here, so one helper
    /// covers every call the connector makes.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<T, SlackError> {
        let body = self.call_text(method, params).await?;
        decode_envelope(method, &body)
    }

    /// POST one Web API method and hand back the body undecoded, for the
    /// callers whose decode needs the raw text rather than one envelope.
    async fn call_text(
        &self,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<String, SlackError> {
        let url = format!("{API_ROOT}/{method}");
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(encode_form(params))
            .send()
            .await
            .map_err(|err| SlackError::Transport {
                method: method.to_owned(),
                detail: err.to_string(),
            })?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            return Err(SlackError::RateLimited {
                method: method.to_owned(),
                retry_after: retry_delay(after),
            });
        }

        let body = response.text().await.map_err(|err| SlackError::Transport {
            method: method.to_owned(),
            detail: err.to_string(),
        })?;
        if !status.is_success() {
            return Err(status_failure(method, status, &body));
        }
        Ok(body)
    }

    /// Who this token belongs to. The first thing to prove on a new workspace.
    pub async fn auth_test(&self) -> Result<AuthTest, SlackError> {
        self.call("auth.test", &[]).await
    }

    /// Every conversation the token's user is a member of, paging to the end.
    pub async fn list_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut params =
                vec![("types", CONVERSATION_TYPES.to_owned()), ("limit", "200".to_owned())];
            if let Some(cursor) = &cursor {
                params.push(("cursor", cursor.clone()));
            }
            let body = self.call_text("users.conversations", &params).await?;
            let page = decode_conversations_page(&body)?;
            out.extend(page.conversations);
            pages += 1;
            match page.next_cursor {
                Some(_) if pages >= MAX_CONVERSATION_PAGES => {
                    tracing::warn!(
                        target: "forge_connectors::slack",
                        pages,
                        conversations = out.len(),
                        "users.conversations kept handing back a cursor; stopping the walk",
                    );
                    return Ok(out);
                }
                Some(next) => cursor = Some(next),
                None => return Ok(out),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned against literals, not against `RETRY_FALLBACK` / `RETRY_CAP`:
    /// asserting the output equals the constant the function itself applies
    /// holds for any value, so it cannot catch a changed cap.
    #[test]
    fn retry_delay_honours_the_header_and_floors_absurd_values() {
        assert_eq!(retry_delay(Some(7)), Duration::from_secs(7), "an honest header is honoured");
        assert_eq!(retry_delay(None), Duration::from_secs(5), "a missing header falls back");
        // A hostile or broken header must not park the pump for an hour.
        assert_eq!(
            retry_delay(Some(100_000)),
            Duration::from_secs(60),
            "an absurd header is capped"
        );
        // `Retry-After: 0` is legal, and retrying immediately is the one
        // thing the header asked us not to do.
        assert_eq!(retry_delay(Some(0)), Duration::from_secs(1), "a zero header still waits");
    }

    #[test]
    fn a_gateway_failure_reports_the_status_and_a_bounded_body_prefix() {
        let mut body = "bad gateway from the edge".to_owned();
        body.push_str(&"x".repeat(1000));
        let err = status_failure("users.conversations", reqwest::StatusCode::BAD_GATEWAY, &body);
        match err {
            SlackError::Transport { method, detail } => {
                assert!(detail.contains("502"), "the status is named, got: {detail}");
                assert!(detail.contains("bad gateway"), "the body prefix is carried: {detail}");
                assert!(detail.len() < 300, "the body is bounded, got {} chars", detail.len());
                assert_eq!(method, "users.conversations");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn an_ok_false_envelope_is_an_api_error_not_a_decode_error() {
        let body = r#"{"ok":false,"error":"missing_scope","needed":"search:read.public"}"#;
        let err = decode_envelope::<serde_json::Value>("conversations.history", body)
            .expect_err("ok:false must be an error");
        match err {
            SlackError::Api { method, error, needed } => {
                assert_eq!(method, "conversations.history");
                assert_eq!(error, "missing_scope");
                assert_eq!(needed.as_deref(), Some("search:read.public"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn an_ok_true_envelope_decodes_its_payload() {
        let body = r#"{"ok":true,"team":"Trust Machines","user":"ved","team_id":"T1","user_id":"U1","url":"https://x.slack.com/"}"#;
        let got: AuthTest = decode_envelope("auth.test", body).expect("decodes");
        assert_eq!(got.team_id, "T1", "the flattened payload carries team_id");
        assert_eq!(got.user, "ved", "the flattened payload carries user");
    }

    #[test]
    fn list_conversations_decodes_mixed_types_and_keeps_the_cursor() {
        let body = r#"{
          "ok": true,
          "channels": [
            {"id":"C1","name":"general","is_channel":true},
            {"id":"D1","is_im":true,"user":"U9"},
            {"id":"G1","name":"secret","is_private":true}
          ],
          "response_metadata": {"next_cursor": "abc123"}
        }"#;
        let page = decode_conversations_page(body).expect("decodes");
        assert_eq!(page.conversations.len(), 3, "every channel in the page is kept");
        assert_eq!(page.conversations[1].user.as_deref(), Some("U9"), "a DM keeps its partner");
        assert_eq!(page.next_cursor.as_deref(), Some("abc123"), "a non-empty cursor pages on");

        let empty = decode_conversations_page(
            r#"{"ok":true,"channels":[],"response_metadata":{"next_cursor":""}}"#,
        )
        .expect("an empty cursor means the last page");
        assert_eq!(empty.next_cursor, None, "an empty cursor ends the walk");
    }

    /// The input class a message body actually contains, and the class a
    /// hand-rolled encoder silently splits a parameter on.
    #[test]
    fn encode_form_escapes_what_a_message_body_carries() {
        let encoded = encode_form(&[("text", "a&b=c+d\ncafé".to_owned())]);
        assert_eq!(encoded, "text=a%26b%3Dc%2Bd%0Acaf%C3%A9");
        assert_eq!(
            encode_form(&[("text", "two words".to_owned())]),
            "text=two+words",
            "a space is a form `+`, not %20"
        );
    }

    #[test]
    fn encode_form_keeps_an_empty_value_and_emits_nothing_for_no_params() {
        assert_eq!(encode_form(&[]), "", "no params is an empty body");
        assert_eq!(
            encode_form(&[("cursor", String::new())]),
            "cursor=",
            "an empty value still emits its name"
        );
    }

    #[test]
    fn a_token_never_reaches_the_debug_output() {
        let client = SlackClient::new(reqwest::Client::new(), "xoxp-supersecret".to_owned());
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("supersecret"), "token leaked: {rendered}");
    }
}
