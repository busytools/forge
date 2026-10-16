//! The HTTP client: one POST per question, everything failed mapped to
//! a typed error, no retries - the caller decides what a failure means.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::{SystemOneConfig, systemone_url};
use crate::error::SystemOneError;
use crate::wire::{Answer, AskOutcome, Question, Usage, validate_answer};

/// The question name on the wire. The name is echoed back, never sent to
/// the model, so one fixed key is enough.
const QUESTION_KEY: &str = "q";

/// How much of a provider error body the error text carries.
const ERROR_BODY_LIMIT: usize = 500;

/// Calls one System One endpoint with one configured model.
pub struct SystemOneClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    timeout: Duration,
}

#[derive(serde::Deserialize)]
struct ResponseEnvelope {
    model: String,
    answers: BTreeMap<String, Answer>,
    usage: Option<Usage>,
}

impl SystemOneClient {
    /// The caller builds the `reqwest::Client`, so a boot site can pass
    /// the `NODE_EXTRA_CA_CERTS` trust client every outbound call uses;
    /// the config's timeout is applied per request so a client shared
    /// with another leg cannot widen it.
    pub fn new(config: &SystemOneConfig, http: reqwest::Client) -> Self {
        Self {
            http,
            endpoint: systemone_url(&config.base_url),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            timeout: Duration::from_millis(config.timeout_ms),
        }
    }

    /// Ask one question about one state; the answer must validate against
    /// the question it answers.
    pub async fn ask(
        &self,
        state: &serde_json::Value,
        question: &Question,
    ) -> Result<AskOutcome, SystemOneError> {
        let body = serde_json::json!({
            "model": self.model,
            "state": state,
            "questions": { QUESTION_KEY: question },
        });
        let mut request = self.http.post(&self.endpoint).json(&body).timeout(self.timeout);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(|err| transport_or_timeout(&err))?;
        let status = response.status();
        let text = response.text().await.map_err(|err| transport_or_timeout(&err))?;
        if !status.is_success() {
            return Err(SystemOneError::Http {
                status: status.as_u16(),
                body: truncate(&self.redact(&text), ERROR_BODY_LIMIT),
            });
        }
        let envelope: ResponseEnvelope = serde_json::from_str(&text).map_err(|err| {
            // A serde message quotes the body it was reading, so the same
            // mask covers it.
            SystemOneError::InvalidResponse(self.redact(&format!("response did not parse: {err}")))
        })?;
        let ResponseEnvelope { model, mut answers, usage } = envelope;
        let answer = match (answers.len(), answers.remove(QUESTION_KEY)) {
            (1, Some(answer)) => answer,
            (1, None) => {
                return Err(SystemOneError::InvalidResponse(format!(
                    "response answers are not keyed `{QUESTION_KEY}`, the one question requested"
                )));
            }
            (count, _) => {
                return Err(SystemOneError::InvalidResponse(format!(
                    "response carries {count} answers where exactly one was requested"
                )));
            }
        };
        // The detail quotes provider values - the choice, the legend and
        // probability keys - so it is masked like the body.
        validate_answer(question, &answer)
            .map_err(|detail| SystemOneError::InvalidResponse(self.redact(&detail)))?;
        Ok(AskOutcome { model, answer, usage })
    }

    /// The provider's text with the configured key's copies masked; no key
    /// configured means nothing to mask.
    fn redact(&self, text: &str) -> String {
        match self.api_key.as_deref() {
            Some(key) => redact_key(text, key),
            None => text.to_owned(),
        }
    }
}

/// A request failure reads as a timeout wherever it struck - the send
/// or the body read - so a stalled connection never surfaces as a
/// decode error.
fn transport_or_timeout(err: &reqwest::Error) -> SystemOneError {
    if err.is_timeout() {
        SystemOneError::Timeout
    } else {
        SystemOneError::Transport(err.to_string())
    }
}

/// The width of the key piece a mask matches on: long enough that it is
/// key material rather than a scheme, short enough to survive a truncation.
const TAG: usize = 8;

/// Text with the credential's copies masked, before the body is cut - a
/// key straddling the cut would otherwise survive as a fragment of itself.
/// A text carrying no key material comes back untouched, its own formatting
/// intact.
///
/// The error text reaches tool output and the transcript JSONL, neither of
/// which rotates, so a host reflecting the request (the key rides its auth)
/// must not put key material there. Two masks cover the shapes a copy
/// arrives in: the key itself wherever it appears, and any word carrying a
/// window of it - a reflected copy can be truncated or wrapped across
/// words, so a word may carry only a piece of the key.
fn redact_key(text: &str, key: &str) -> String {
    // Below the tag's own width the key is a placeholder rather than key
    // material, so only a word that IS the key is masked: masking the
    // words that merely CONTAIN it eats the diagnostics the body exists for.
    let tag: String = key.chars().take(TAG).collect();
    if tag.chars().count() < TAG {
        if key.is_empty() {
            return text.to_owned();
        }
        if !text.split_whitespace().any(|word| bare_word(word) == key) {
            return text.to_owned();
        }
        return text
            .split_whitespace()
            .map(|word| if bare_word(word) == key { "[redacted]" } else { word })
            .collect::<Vec<_>>()
            .join(" ");
    }
    // Every window of the key, not just its opening: a copy can be
    // truncated or wrapped, so the piece that survives still carries a
    // window of it.
    let windows: Vec<String> =
        key.chars().collect::<Vec<_>>().windows(TAG).map(|w| w.iter().collect()).collect();
    if !windows.iter().any(|window| text.contains(window)) {
        return text.to_owned();
    }
    // Whitespace runs normalize in a text that carried key material.
    text.replace(key, "[redacted]")
        .split_whitespace()
        .map(|word| if windows.iter().any(|w| word.contains(w)) { "[redacted]" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A word with the punctuation a key cannot hold trimmed off its ends, the
/// way a JSON body carries one (`"hunter2",` reads as the key).
fn bare_word(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
}

/// A bounded prefix of a provider error body, cut on a char boundary.
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &text[..cut])
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Mutex;

    use super::*;
    use crate::wire::NoulCriteria;

    /// One request the test server saw.
    #[derive(Debug)]
    struct Recorded {
        body: serde_json::Value,
        authorization: Option<String>,
    }

    /// Spawn a one-route server that records what it was sent and answers
    /// with the canned (status, body), optionally after a delay.
    async fn spawn_server(
        status: u16,
        response_body: &str,
        delay: Option<Duration>,
    ) -> (String, Arc<Mutex<Vec<Recorded>>>) {
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&requests);
        let body_for_handler: Arc<String> = Arc::new(response_body.to_owned());
        let app = axum::Router::new().route(
            "/v1/systemone",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let recorder = Arc::clone(&recorder);
                let body_for_handler = Arc::clone(&body_for_handler);
                async move {
                    let parsed = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    let authorization = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    recorder.lock().await.push(Recorded { body: parsed, authorization });
                    if let Some(delay) = delay {
                        tokio::time::sleep(delay).await;
                    }
                    (
                        axum::http::StatusCode::from_u16(status).expect("test status"),
                        (*body_for_handler).clone(),
                    )
                }
            }),
        );
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test listener binds");
        let addr = listener.local_addr().expect("bound address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server serves");
        });
        (format!("http://127.0.0.1:{}", addr.port()), requests)
    }

    fn config_for(base: &str, api_key: Option<&str>) -> SystemOneConfig {
        SystemOneConfig {
            base_url: base.to_owned(),
            api_key: api_key.map(str::to_owned),
            model: "test-model".to_owned(),
            timeout_ms: 5_000,
        }
    }

    fn client_for(base: &str, api_key: Option<&str>) -> SystemOneClient {
        SystemOneClient::new(&config_for(base, api_key), reqwest::Client::new())
    }

    fn noul_question() -> Question {
        Question::Noul { instructions: serde_json::json!("Is this a bug?"), criteria: None }
    }

    fn choice_question() -> Question {
        Question::Choice {
            instructions: serde_json::json!("Which team?"),
            criteria: [
                ("billing".to_owned(), serde_json::Value::Null),
                ("technical".to_owned(), serde_json::Value::Null),
            ]
            .into_iter()
            .collect(),
        }
    }

    /// Ok(()) means the answer validated; the canned Noul answers 0.5.
    const NOUL_OK_BODY: &str = r#"{"model":"test-model","answers":{"q":{"type":"noul","noul":0.83}},"usage":{"input_tokens":10,"output_tokens":3}}"#;

    fn http_error(err: SystemOneError) -> (u16, String) {
        match err {
            SystemOneError::Http { status, body } => (status, body),
            other => panic!("expected an Http error, got {other:?}"),
        }
    }

    fn invalid_response(err: SystemOneError) -> String {
        match err {
            SystemOneError::InvalidResponse(detail) => detail,
            other => panic!("expected an InvalidResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn noul_round_trips_and_sends_the_wire_body() {
        let (base, requests) = spawn_server(200, NOUL_OK_BODY, None).await;
        let client = client_for(&base, Some("k"));

        let outcome = client
            .ask(&serde_json::json!({"ticket": "x"}), &noul_question())
            .await
            .expect("request succeeds");

        assert_eq!(
            outcome,
            AskOutcome {
                model: "test-model".to_owned(),
                answer: Answer::Noul { noul: 0.83 },
                usage: Some(Usage { input_tokens: 10, output_tokens: 3, cost: None }),
            }
        );
        let seen = requests.lock().await;
        assert_eq!(
            seen[0].body,
            serde_json::json!({"model":"test-model","state":{"ticket":"x"},"questions":{"q":{"type":"noul","instructions":"Is this a bug?"}}})
        );
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer k"));
    }

    /// The API's own typing carried end to end: structured `instructions`
    /// and criteria values reach the request body verbatim.
    #[tokio::test]
    async fn structured_question_reaches_the_request_body() {
        let (base, requests) = spawn_server(200, NOUL_OK_BODY, None).await;
        let client = client_for(&base, Some("k"));

        let noul = Question::Noul {
            instructions: serde_json::json!({
                "question": "Is the claim `just check` green?",
                "evidence": {"command": "just check", "verdict": "all green"}
            }),
            criteria: Some(NoulCriteria {
                r#true: serde_json::json!({"rule": "the verdict line says all green"}),
                r#false: serde_json::json!("anything else"),
            }),
        };
        client
            .ask(&serde_json::json!({"ticket": "x"}), &noul)
            .await
            .expect("the structured noul request succeeds");

        let choice = Question::Choice {
            instructions: serde_json::json!("Which team owns `auth.rs`?"),
            criteria: [
                ("billing".to_owned(), serde_json::json!({"files": ["a.rs"]})),
                ("frontend".to_owned(), serde_json::Value::Null),
            ]
            .into_iter()
            .collect(),
        };
        let choice_body = r#"{"model":"test-model","answers":{"q":{"type":"choice","choice":"billing"}},"usage":{"input_tokens":10,"output_tokens":3}}"#;
        let (choice_base, choice_requests) = spawn_server(200, choice_body, None).await;
        client_for(&choice_base, Some("k"))
            .ask(&serde_json::json!("x"), &choice)
            .await
            .expect("the structured choice request succeeds");

        let seen = requests.lock().await;
        assert_eq!(
            seen[0].body,
            serde_json::json!({
                "model": "test-model",
                "state": {"ticket": "x"},
                "questions": {"q": {
                    "type": "noul",
                    "instructions": {
                        "question": "Is the claim `just check` green?",
                        "evidence": {"command": "just check", "verdict": "all green"}
                    },
                    "criteria": {
                        "true": {"rule": "the verdict line says all green"},
                        "false": "anything else"
                    }
                }}
            }),
            "structured noul values reach the request body verbatim"
        );
        let choice_seen = choice_requests.lock().await;
        assert_eq!(
            choice_seen[0].body,
            serde_json::json!({
                "model": "test-model",
                "state": "x",
                "questions": {"q": {
                    "type": "choice",
                    "instructions": "Which team owns `auth.rs`?",
                    "criteria": {"billing": {"files": ["a.rs"]}, "frontend": null}
                }}
            }),
            "structured choice option values reach the request body verbatim"
        );
    }

    #[tokio::test]
    async fn no_key_sends_no_authorization_header() {
        let (base, requests) = spawn_server(200, NOUL_OK_BODY, None).await;
        let client = client_for(&base, None);

        client.ask(&serde_json::json!("x"), &noul_question()).await.expect("request succeeds");

        let seen = requests.lock().await;
        assert_eq!(seen[0].authorization, None);
    }

    #[tokio::test]
    async fn typesafe_detail_body_is_carried() {
        let body =
            r#"{"detail":[{"type":"missing","loc":["body","model"],"msg":"Field required"}]}"#;
        let (base, _) = spawn_server(422, body, None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("422 is an error");

        let (status, body) = http_error(err);
        assert_eq!(status, 422);
        assert!(body.contains("\"missing\""), "{body}");
    }

    #[tokio::test]
    async fn openrouter_error_envelope_is_carried() {
        let body = r#"{"error":{"code":429,"message":"Rate limit exceeded"}}"#;
        let (base, _) = spawn_server(429, body, None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("429 is an error");

        let (status, body) = http_error(err);
        assert_eq!(status, 429);
        assert!(body.contains("Rate limit exceeded"), "{body}");
    }

    #[tokio::test]
    async fn html_error_body_is_carried() {
        let (base, _) = spawn_server(504, "<html><body>gateway timeout</body></html>", None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("504 is an error");

        let (status, body) = http_error(err);
        assert_eq!(status, 504);
        assert!(body.starts_with("<html>"), "{body}");
    }

    #[tokio::test]
    async fn a_dead_port_is_a_transport_error() {
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test listener binds");
        let addr = listener.local_addr().expect("bound address");
        drop(listener);
        let client = client_for(&format!("http://127.0.0.1:{}", addr.port()), Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("refused connection is an error");

        assert!(matches!(err, SystemOneError::Transport(_)), "{err:?}");
    }

    /// A server that sends headers and half a body, then stalls: the
    /// stall sits inside the body read, and it must still read as a
    /// timeout - not as a transport failure.
    #[tokio::test]
    async fn a_stalled_body_read_times_out() {
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test listener binds");
        let addr = listener.local_addr().expect("bound address");
        let hold = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut stream, _) = listener.accept().await.expect("accept");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"model\":")
                .await
                .expect("write headers and a partial body");
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let mut config = config_for(&format!("http://127.0.0.1:{}", addr.port()), Some("k"));
        config.timeout_ms = 100;
        let client = SystemOneClient::new(&config, reqwest::Client::new());

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("a stalled body is an error");

        assert!(matches!(err, SystemOneError::Timeout), "{err:?}");
        hold.abort();
    }

    #[tokio::test]
    async fn a_slow_server_times_out() {
        let (base, _) = spawn_server(200, NOUL_OK_BODY, Some(Duration::from_millis(200))).await;
        let mut config = config_for(&base, Some("k"));
        config.timeout_ms = 50;
        let client = SystemOneClient::new(&config, reqwest::Client::new());

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("a slow response is an error");

        assert!(matches!(err, SystemOneError::Timeout), "{err:?}");
    }

    #[tokio::test]
    async fn choice_key_drift_is_an_invalid_response() {
        let body = r#"{"model":"m","answers":{"q":{"type":"choice","choice":"billing","probabilities":{"billing":0.9,"sales":0.1}}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
        let (base, _) = spawn_server(200, body, None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &choice_question())
            .await
            .expect_err("drifted keys are an error");

        assert!(invalid_response(err).starts_with("probability keys"));
    }

    #[tokio::test]
    async fn extra_response_fields_are_ignored() {
        let body = r#"{"id":"gen-1","provider":"TypeSafe","model":"m","answers":{"q":{"type":"noul","noul":0.5}},"usage":{"input_tokens":1,"output_tokens":1,"cost":0.0001}}"#;
        let (base, _) = spawn_server(200, body, None).await;
        let client = client_for(&base, Some("k"));

        let outcome = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect("extra fields do not break parsing");

        assert_eq!(outcome.usage.expect("usage present").cost, Some(0.0001));
    }

    #[tokio::test]
    async fn missing_usage_leaves_the_answer_standing() {
        let body = r#"{"model":"m","answers":{"q":{"type":"noul","noul":0.5}}}"#;
        let (base, _) = spawn_server(200, body, None).await;
        let client = client_for(&base, Some("k"));

        let outcome = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect("a delivered decision stands without usage");

        assert_eq!(outcome.usage, None, "an absent usage is None, never invented zeros");
        assert_eq!(outcome.answer, Answer::Noul { noul: 0.5 });
    }

    #[tokio::test]
    async fn a_long_error_body_is_bounded_and_marked() {
        let body = format!("START {} END", "x".repeat(10_000));
        let (base, _) = spawn_server(500, &body, None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("500 is an error");

        let (_, text) = http_error(err);
        assert!(text.starts_with("START "), "{text}");
        assert!(text.ends_with("..."), "a bounded snippet says it was cut: {text}");
        assert!(text.len() <= 503, "the snippet stays bounded: {}", text.len());
    }

    /// The cut walks back to a char boundary; a boundary-unaware slice
    /// would panic right here on the multi-byte run the limit lands in,
    /// which is the property this test rests on.
    #[tokio::test]
    async fn a_multibyte_error_body_cuts_on_a_char_boundary() {
        let body = format!("A{}Z", "é".repeat(1000));
        let (base, _) = spawn_server(500, &body, None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("500 is an error");

        let (_, text) = http_error(err);
        assert!(text.starts_with('A'), "{text}");
        assert!(text.ends_with("..."), "{text}");
    }

    /// The error body reaches tool output and the transcript JSONL, which
    /// do not rotate: a host reflecting the request (the key rides its
    /// auth) must never print key material there - the key itself, and a
    /// copy wrapped across words, each piece carrying a run of the key.
    #[tokio::test]
    async fn a_reflected_key_in_an_error_body_never_prints() {
        let key = "sk-live-EXAMPLEKEYVALUE";
        let body = format!(
            "origin echoed the request: {key} and the header \"Authorization: Bearer {key}\" \
             and a wrapped echo: {} {}",
            &key[..12],
            &key[12..]
        );
        let (base, _) = spawn_server(502, &body, None).await;
        let client = client_for(&base, Some(key));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("502 is an error");

        let (status, text) = http_error(err);
        assert_eq!(status, 502);
        assert!(!text.contains(key), "the key itself never prints: {text}");
        assert!(!text.contains(&key[..12]), "a wrapped copy's head never prints: {text}");
        assert!(!text.contains(&key[12..]), "a wrapped copy's tail never prints: {text}");
        assert!(
            text.contains("\"Authorization: Bearer [redacted]\""),
            "the verbatim echo is masked in place, its header kept: {text}"
        );
        assert!(
            text.contains("origin echoed the request"),
            "the body is masked, not dropped: {text}"
        );
    }

    /// The parse-failure detail quotes the body serde was reading, and it
    /// lands in the same tool output: the key is masked there too.
    #[tokio::test]
    async fn a_reflected_key_in_a_parse_failure_never_prints() {
        let key = "sk-live-EXAMPLEKEYVALUE";
        let (base, _) = spawn_server(200, &format!("\"upstream echoed: {key}\""), None).await;
        let client = client_for(&base, Some(key));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("a body that is not an envelope is an error");

        let detail = invalid_response(err);
        assert!(
            detail.starts_with("response did not parse"),
            "a parse failure is still named as one"
        );
        assert!(!detail.contains(key), "the parse-failure detail never prints the key");
    }

    /// The validation detail is built from provider strings - the choice it
    /// was given, legend and probability keys - and it lands in the same
    /// tool output: the key is masked there too.
    #[tokio::test]
    async fn a_reflected_key_in_a_validation_detail_never_prints() {
        let key = "sk-live-EXAMPLEKEYVALUE";
        let body = format!(
            r#"{{"model":"m","answers":{{"q":{{"type":"choice","choice":"{key}"}}}},"usage":null}}"#
        );
        let (base, _) = spawn_server(200, &body, None).await;
        let client = client_for(&base, Some(key));

        let err = client
            .ask(&serde_json::json!("x"), &choice_question())
            .await
            .expect_err("a choice outside the criteria is an error");

        let detail = invalid_response(err);
        assert!(
            detail.contains("not one of the criteria keys"),
            "the validation failure keeps its own wording"
        );
        assert!(!detail.contains(key), "the validation detail never prints the key");
    }

    /// A body carrying no key material comes back byte-identical: the mask
    /// is a no-op there, so a clean body's own newlines and indentation
    /// survive. Each mask path guards its no-op separately - a key at or
    /// over the tag width, and one below it - so both keys run.
    #[tokio::test]
    async fn a_body_without_the_key_is_untouched() {
        let body = "<html>\n  <body>gateway timeout</body>\n</html>";
        for key in ["sk-live-EXAMPLEKEYVALUE", "hunter2"] {
            let (base, _) = spawn_server(504, body, None).await;
            let client = client_for(&base, Some(key));

            let err = client
                .ask(&serde_json::json!("x"), &noul_question())
                .await
                .expect_err("504 is an error");

            let (_, text) = http_error(err);
            assert_eq!(text, body, "a keyless body is not reflowed (key `{key}`)");
        }
    }

    /// A key below the tag width is masked only where a word IS it, so a
    /// placeholder does not eat the diagnostics around it. The masked word
    /// goes whole, punctuation and all (`"hunter2",` reads as the key). The
    /// glued compact-JSON shape (`{"token":"hunter2"}`) stays unmasked by
    /// design: its word is not the key, and masking every word containing
    /// the placeholder is what the width rule avoids.
    #[tokio::test]
    async fn a_short_key_masks_the_word_that_is_it() {
        let (base, _) = spawn_server(500, "upstream said: \"hunter2\", apparently", None).await;
        let client = client_for(&base, Some("hunter2"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("500 is an error");

        let (_, text) = http_error(err);
        assert_eq!(
            text, "upstream said: [redacted] apparently",
            "the word that carries the key is masked whole, its neighbours kept"
        );
    }

    /// The mask runs over the whole body, before the cut: the cut can land
    /// a few characters into the key, below the width the windowed masks
    /// match on, so a key in its reach would otherwise survive as a
    /// fragment of itself.
    #[tokio::test]
    async fn the_key_is_masked_before_the_body_is_cut() {
        let key = "sk-live-EXAMPLEKEYVALUE";
        let body = format!("{}{key} tail", "x".repeat(496));
        let (base, _) = spawn_server(500, &body, None).await;
        let client = client_for(&base, Some(key));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("500 is an error");

        let (_, text) = http_error(err);
        assert!(text.ends_with("..."), "the body reached the cut this pins: {text}");
        assert!(!text.contains(&key[..4]), "no fragment of the key survives the cut: {text}");
    }

    #[tokio::test]
    async fn answer_key_mismatch_is_an_invalid_response() {
        let body = r#"{"model":"m","answers":{"x":{"type":"noul","noul":0.5}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
        let (base, _) = spawn_server(200, body, None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("a mismatched answer key is an error");

        let detail = invalid_response(err);
        assert!(detail.contains("not keyed `q`"), "the wording names what came instead: {detail}");
        assert!(detail.contains("requested"), "and what was asked for: {detail}");
    }

    /// Extra answers beside the requested one: the wording must name the
    /// count, not dress up the present key as the problem.
    #[tokio::test]
    async fn multiple_answers_are_an_invalid_response() {
        let body = r#"{"model":"m","answers":{"q":{"type":"noul","noul":0.5},"extra":{"type":"noul","noul":0.1}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
        let (base, _) = spawn_server(200, body, None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("extra answers are an error");

        let detail = invalid_response(err);
        assert!(detail.contains("carries 2 answers"), "the wording names the count: {detail}");
        assert!(detail.contains("exactly one was requested"), "{detail}");
    }
}
