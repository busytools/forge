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
        let response = request.send().await.map_err(|err| {
            if err.is_timeout() {
                SystemOneError::Timeout
            } else {
                SystemOneError::Transport(err.to_string())
            }
        })?;
        let status = response.status();
        let text =
            response.text().await.map_err(|err| SystemOneError::Transport(err.to_string()))?;
        if !status.is_success() {
            return Err(SystemOneError::Http {
                status: status.as_u16(),
                body: truncate(&text, ERROR_BODY_LIMIT),
            });
        }
        let envelope: ResponseEnvelope = serde_json::from_str(&text).map_err(|err| {
            SystemOneError::InvalidResponse(format!("response did not parse: {err}"))
        })?;
        let ResponseEnvelope { model, mut answers, usage } = envelope;
        let answer = if answers.len() == 1 { answers.remove(QUESTION_KEY) } else { None };
        let Some(answer) = answer else {
            let keys: Vec<&String> = answers.keys().collect();
            return Err(SystemOneError::InvalidResponse(format!(
                "response answers {keys:?} do not match the requested question `{QUESTION_KEY}`"
            )));
        };
        let usage = usage.ok_or_else(|| {
            SystemOneError::InvalidResponse("response is missing `usage`".to_owned())
        })?;
        validate_answer(question, &answer).map_err(SystemOneError::InvalidResponse)?;
        Ok(AskOutcome { model, answer, usage })
    }
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
        Question::Noul { instructions: "Is this a bug?".to_owned(), criteria: None }
    }

    fn choice_question() -> Question {
        Question::Choice {
            instructions: "Which team?".to_owned(),
            criteria: [("billing".to_owned(), None), ("technical".to_owned(), None)]
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
                usage: Usage { input_tokens: 10, output_tokens: 3, cost: None },
            }
        );
        let seen = requests.lock().await;
        assert_eq!(
            seen[0].body,
            serde_json::json!({"model":"test-model","state":{"ticket":"x"},"questions":{"q":{"type":"noul","instructions":"Is this a bug?"}}})
        );
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer k"));
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

        assert_eq!(outcome.usage.cost, Some(0.0001));
    }

    #[tokio::test]
    async fn missing_usage_is_an_invalid_response() {
        let body = r#"{"model":"m","answers":{"q":{"type":"noul","noul":0.5}}}"#;
        let (base, _) = spawn_server(200, body, None).await;
        let client = client_for(&base, Some("k"));

        let err = client
            .ask(&serde_json::json!("x"), &noul_question())
            .await
            .expect_err("missing usage is an error");

        assert!(invalid_response(err).contains("usage"));
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

        assert!(invalid_response(err).contains("requested question"));
    }
}
