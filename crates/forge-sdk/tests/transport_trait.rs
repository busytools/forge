//! Verifies the public [`Transport`] trait + `Client::spawn_with_transport`
//! injection path. Mirrors Python SDK's extensibility surface where
//! users implement `Transport` for custom I/O (remote, in-memory,
//! containerised).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use forge_sdk::{Client, Error, Message, OptionsBuilder, Transport};

/// In-memory transport driven by a scripted queue of stdout lines.
/// Writes are captured so tests can assert what the client sent.
struct MockTransport {
    stdout: Mutex<VecDeque<String>>,
    writes: Mutex<Vec<String>>,
    ended_input: Mutex<bool>,
    closed: Mutex<bool>,
}

impl MockTransport {
    fn new(lines: Vec<&str>) -> Self {
        Self {
            stdout: Mutex::new(lines.into_iter().map(String::from).collect()),
            writes: Mutex::new(Vec::new()),
            ended_input: Mutex::new(false),
            closed: Mutex::new(false),
        }
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn read_line(&mut self) -> Result<Option<String>, Error> {
        Ok(self.stdout.lock().unwrap().pop_front())
    }

    async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        self.writes.lock().unwrap().push(line.to_string());
        // On an initialize request, push the matching control_response
        // onto the stdout queue so the test harness keeps marching.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if value.get("type").and_then(|v| v.as_str()) == Some("control_request") {
                if let Some(req_id) = value.get("request_id").and_then(serde_json::Value::as_str) {
                    let response = format!(
                        r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{req_id}","response":{{}}}}}}"#
                    );
                    self.stdout.lock().unwrap().push_back(response);
                }
            }
        }
        Ok(())
    }

    async fn end_input(&mut self) -> Result<(), Error> {
        *self.ended_input.lock().unwrap() = true;
        Ok(())
    }

    async fn close(&mut self) -> Result<(), Error> {
        *self.closed.lock().unwrap() = true;
        Ok(())
    }
}

/// Simple end-to-end: an injected in-memory transport can drive a
/// full client lifecycle (init → initialize handshake → disconnect).
#[tokio::test]
async fn spawn_with_transport_smoke() {
    let init = r#"{"type":"system","subtype":"init","session_id":"mock-xyz","cwd":"/tmp","tools":[],"mcp_servers":[],"model":"claude","permissionMode":"default","apiKeySource":"env"}"#;
    let mock = Box::new(MockTransport::new(vec![init]));
    let opts = OptionsBuilder::new().binary("unused").build();
    let client = Client::spawn_with_transport(opts, mock)
        .await
        .expect("spawn_with_transport");
    assert_eq!(client.session_id(), "mock-xyz");
    client.disconnect().await.expect("disconnect");
}

/// Custom transport sees the initialize `control_request` written
/// after the init line is drained.
#[tokio::test]
async fn spawn_with_transport_sends_initialize() {
    // Capture a reference to the mock before boxing so we can inspect
    // the writes after disconnect.
    use std::sync::Arc;
    let mock = Arc::new(MockTransportShared::default());
    mock.seed_stdout(r#"{"type":"system","subtype":"init","session_id":"s","cwd":"/tmp","tools":[],"mcp_servers":[],"model":"claude","permissionMode":"default","apiKeySource":"env"}"#);
    let boxed: Box<dyn Transport> = Box::new(MockTransportShared::clone_arc(&mock));
    let client =
        Client::spawn_with_transport(OptionsBuilder::new().binary("unused").build(), boxed)
            .await
            .expect("spawn");
    client.disconnect().await.expect("disconnect");

    let writes = mock.writes.lock().unwrap();
    let init_req_seen = writes.iter().any(|l| {
        l.contains("\"type\":\"control_request\"") && l.contains("\"subtype\":\"initialize\"")
    });
    assert!(
        init_req_seen,
        "client must send the initialize control_request through the injected transport; got: {:?}",
        &*writes
    );
    assert!(
        *mock.closed.lock().unwrap(),
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
    let mock = Box::new(MockTransport::new(vec![init, drift]));
    let opts = OptionsBuilder::new().binary("unused").build();
    let mut client = Client::spawn_with_transport(opts, mock)
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

// Arc-shared mock so the test can retain a handle after boxing for
// assertions.
#[derive(Default)]
struct MockTransportShared {
    stdout: Mutex<VecDeque<String>>,
    writes: Mutex<Vec<String>>,
    closed: Mutex<bool>,
}

impl MockTransportShared {
    fn seed_stdout(&self, line: &str) {
        self.stdout.lock().unwrap().push_back(line.to_string());
    }
    fn clone_arc(arc: &std::sync::Arc<Self>) -> MockTransportShareHandle {
        MockTransportShareHandle(arc.clone())
    }
}

struct MockTransportShareHandle(std::sync::Arc<MockTransportShared>);

#[async_trait]
impl Transport for MockTransportShareHandle {
    async fn read_line(&mut self) -> Result<Option<String>, Error> {
        Ok(self.0.stdout.lock().unwrap().pop_front())
    }
    async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        self.0.writes.lock().unwrap().push(line.to_string());
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if value.get("type").and_then(|v| v.as_str()) == Some("control_request") {
                if let Some(req_id) = value.get("request_id").and_then(serde_json::Value::as_str) {
                    let response = format!(
                        r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{req_id}","response":{{}}}}}}"#
                    );
                    self.0.stdout.lock().unwrap().push_back(response);
                }
            }
        }
        Ok(())
    }
    async fn end_input(&mut self) -> Result<(), Error> {
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        *self.0.closed.lock().unwrap() = true;
        Ok(())
    }
}
