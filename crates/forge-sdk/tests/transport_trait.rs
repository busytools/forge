//! Verifies the public [`Transport`] trait + `Client::spawn_with_transport`
//! injection path. Used by `forge-test-harness`'s wire-recording
//! transport for conformance baselines.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge_sdk::{AsyncWriter, Client, Error, Message, OptionsBuilder, Transport};

/// Shared state between the [`MockTransport`] read side and its
/// clonable [`MockWriter`]. The mock auto-replies to any
/// `initialize` `control_request` by pushing a success response onto
/// the stdout queue, so the init handshake completes without
/// scripted choreography.
#[derive(Default, Debug)]
struct MockState {
    stdout: Mutex<VecDeque<String>>,
    writes: Mutex<Vec<String>>,
    closed: Mutex<bool>,
}

impl MockState {
    fn seed(&self, line: &str) {
        self.stdout.lock().unwrap().push_back(line.to_string());
    }

    fn record_write_and_auto_reply(&self, line: &str) {
        self.writes.lock().unwrap().push(line.to_string());
        // On any outbound control_request, push a matching success
        // control_response onto the stdout queue. This makes the
        // mock work for the init handshake AND any in-flight
        // send_control calls without per-test scripting.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if value.get("type").and_then(serde_json::Value::as_str) == Some("control_request") {
                if let Some(req_id) = value.get("request_id").and_then(serde_json::Value::as_str) {
                    let response = format!(
                        r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{req_id}","response":{{}}}}}}"#
                    );
                    self.stdout.lock().unwrap().push_back(response);
                }
            }
        }
    }
}

struct MockTransport {
    state: Arc<MockState>,
}

impl MockTransport {
    fn new(lines: Vec<&str>) -> (Self, Arc<MockState>) {
        let state = Arc::new(MockState::default());
        for line in lines {
            state.seed(line);
        }
        (
            Self {
                state: state.clone(),
            },
            state,
        )
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn read_line(&mut self) -> Result<Option<String>, Error> {
        Ok(self.state.stdout.lock().unwrap().pop_front())
    }

    async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        self.state.record_write_and_auto_reply(line);
        Ok(())
    }

    async fn end_input(&mut self) -> Result<(), Error> {
        Ok(())
    }

    async fn close(&mut self) -> Result<(), Error> {
        *self.state.closed.lock().unwrap() = true;
        Ok(())
    }

    fn try_clone_writer(&self) -> Option<Arc<dyn AsyncWriter>> {
        Some(Arc::new(MockWriter {
            state: self.state.clone(),
        }))
    }
}

#[derive(Debug)]
struct MockWriter {
    state: Arc<MockState>,
}

#[async_trait]
impl AsyncWriter for MockWriter {
    async fn write_line(&self, line: &str) -> Result<(), Error> {
        self.state.record_write_and_auto_reply(line);
        Ok(())
    }

    async fn end_input(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// Simple end-to-end: an injected in-memory transport can drive a
/// full client lifecycle (init → initialize handshake → disconnect).
#[tokio::test]
async fn spawn_with_transport_smoke() {
    let init = r#"{"type":"system","subtype":"init","session_id":"mock-xyz","cwd":"/tmp","tools":[],"mcp_servers":[],"model":"claude","permissionMode":"default","apiKeySource":"env"}"#;
    let (mock, _state) = MockTransport::new(vec![init]);
    let opts = OptionsBuilder::new().binary("unused").build();
    let client = Client::spawn_with_transport(opts, Box::new(mock))
        .await
        .expect("spawn_with_transport");
    assert_eq!(client.session_id(), "mock-xyz");
    client.disconnect().await.expect("disconnect");
}

/// Custom transport sees the initialize `control_request` written
/// after the init line is drained.
#[tokio::test]
async fn spawn_with_transport_sends_initialize() {
    let init = r#"{"type":"system","subtype":"init","session_id":"s","cwd":"/tmp","tools":[],"mcp_servers":[],"model":"claude","permissionMode":"default","apiKeySource":"env"}"#;
    let (mock, state) = MockTransport::new(vec![init]);
    let client = Client::spawn_with_transport(
        OptionsBuilder::new().binary("unused").build(),
        Box::new(mock),
    )
    .await
    .expect("spawn");
    client.disconnect().await.expect("disconnect");

    let writes = state.writes.lock().unwrap();
    let init_req_seen = writes.iter().any(|l| {
        l.contains("\"type\":\"control_request\"") && l.contains("\"subtype\":\"initialize\"")
    });
    assert!(
        init_req_seen,
        "client must send the initialize control_request through the injected transport; got: {:?}",
        &*writes
    );
    assert!(
        *state.closed.lock().unwrap(),
        "client must call close() on the transport during disconnect"
    );
}

/// `next_event` surfaces a `Message::Unknown` when the CLI emits a
/// frame whose top-level `type` value forge-sdk doesn't recognise —
/// the programmatic forward-compat surface library consumers use to
/// detect upstream drift.
#[tokio::test]
async fn next_event_surfaces_unknown_top_level_type() {
    let init = r#"{"type":"system","subtype":"init","session_id":"mock-zzz","cwd":"/tmp","tools":[],"mcp_servers":[],"model":"claude","permissionMode":"default","apiKeySource":"env"}"#;
    let drift = r#"{"type":"future_thing","subtype":"experimental","payload":{"k":"v"}}"#;
    let (mock, _state) = MockTransport::new(vec![init, drift]);
    let opts = OptionsBuilder::new().binary("unused").build();
    let client = Client::spawn_with_transport(opts, Box::new(mock))
        .await
        .expect("spawn_with_transport");

    let msg = client
        .next_event()
        .await
        .expect("next_event ok")
        .expect("frame");
    match msg {
        Message::Unknown { type_str, raw } => {
            assert_eq!(type_str, "future_thing");
            assert_eq!(
                raw.get("subtype").and_then(|v| v.as_str()),
                Some("experimental")
            );
            assert_eq!(
                raw.get("payload")
                    .and_then(|v| v.get("k"))
                    .and_then(|v| v.as_str()),
                Some("v")
            );
        }
        other => panic!("expected Message::Unknown, got: {other:?}"),
    }
    client.disconnect().await.expect("disconnect");
}
