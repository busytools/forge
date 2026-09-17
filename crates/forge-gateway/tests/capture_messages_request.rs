//! Captures a real `claude`-emitted Messages request body through the
//! gateway, for the splice tests to parse. Ignored by default: run it
//! once with `cargo test -p forge-gateway --test capture_messages_request -- --ignored`
//! after a CLI bump, then commit the redacted fixture.
//!
//! The stub upstream answers 401, so nothing is billed - the capture is
//! of the request the CLI emits before any auth check. Redaction walks
//! the parsed JSON and rewrites string values only: a byte-level pass
//! ate numeric literals (12+ decimal digits ARE hex digits) and
//! corrupted the fixture outside every string.

#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use forge_gateway::account::AccountKey;
use forge_gateway::binding::Registration;
use forge_gateway::forward::Gateway;
use forge_gateway::listener::{GatewayListener, RouteHandler, StreamBody};
use forge_primitives::account::{LoadedAccount, Provider};
use http_body_util::BodyExt;

/// One recorded upstream request.
#[derive(Default, Clone)]
struct CaptureUpstream {
    body: Arc<parking_lot::Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl RouteHandler for CaptureUpstream {
    async fn route(&self, request: hyper::Request<Bytes>) -> hyper::Response<StreamBody> {
        *self.body.lock() = Some(String::from_utf8_lossy(request.body()).into_owned());
        let response = hyper::Response::builder().status(hyper::StatusCode::UNAUTHORIZED);
        response.body(error_body()).expect("static response parts always build")
    }
}

fn error_body() -> StreamBody {
    http_body_util::combinators::BoxBody::new(
        http_body_util::Full::new(Bytes::from_static(
            b"{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\"}}",
        ))
        .map_err(|never| match never {}),
    )
}

/// Capture-machine identifiers - the real name, home path in both the
/// plain and the dash-sanitised form the CLI derives its project slugs
/// from, email, city, the throwaway config dir the capture spawns
/// under, and long hex-shaped runs (UUIDs, the CLI's 64-hex device id,
/// hashes) - are capture-local. The bare GitHub username, org, account
/// and project names are public and stay.
fn redact(text: &str, config_dir: &str) -> String {
    let mut out = text.to_owned();
    for secret in [
        "/Users/vedhavyas",
        "-Users-vedhavyas",
        "7549475+vedhavyas@users.noreply.github.com",
        "Vedhavyas Singareddi",
        "Hyderabad",
    ] {
        out = out.replace(secret, "<REDACTED>");
    }
    if !config_dir.is_empty() {
        out = out.replace(config_dir, "<REDACTED>");
    }
    while let Some((start, len)) = find_hex_run(&out) {
        out.replace_range(start..start + len, "<UUID>");
    }
    out
}

fn find_hex_run(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_hexdigit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_hexdigit() || bytes[i] == b'-') {
                i += 1;
            }
            let len = i - start;
            // Any 12+ char hex-shaped run is capture-local: UUIDs, the
            // CLI's 64-hex device id, long hashes. Dash or no dash.
            if len >= 12 {
                return Some((start, len));
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Walk the parsed capture and redact string values only, so numeric
/// literals and JSON structure survive the pass untouched.
fn redact_string_values(value: &mut serde_json::Value, config_dir: &str) {
    match value {
        serde_json::Value::String(text) => *text = redact(text, config_dir),
        serde_json::Value::Array(items) => {
            for item in items {
                redact_string_values(item, config_dir);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                redact_string_values(item, config_dir);
            }
        }
        _ => {}
    }
}

#[tokio::test]
#[ignore = "live capture: spawns the real claude CLI; run with --ignored after a CLI bump"]
async fn capture_a_real_messages_request_body() {
    let upstream = Arc::new(CaptureUpstream::default());
    let stub_port = free_port().await;
    let stub_listener = GatewayListener::bind(stub_port).await.expect("stub bind");
    let stub_url = format!("http://127.0.0.1:{stub_port}");
    let upstream_handler: Arc<dyn RouteHandler> = upstream.clone();
    tokio::spawn(async move { stub_listener.run(upstream_handler).await });

    let account_env = HashMap::from([
        // The upstream is the local stub: the capture stays local and
        // the stub 401s the request, so nothing is billed.
        ("ANTHROPIC_BASE_URL".to_owned(), stub_url.clone()),
        // A capture-shaped credential: the gateway attaches it and the
        // stub 401s it. The real setup token is never involved.
        ("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "capture-credential".to_owned()),
    ]);
    let pool = Arc::new(forge_gateway::AccountPool::new(&[LoadedAccount {
        display_name: "Capture".to_owned(),
        provider: Provider::Anthropic,
        base_url: None,
        models: vec!["claude-sonnet-5".to_owned()],
        model_aliases: std::collections::HashMap::new(),
        model_slugs: std::collections::HashMap::new(),
        env: account_env.clone(),
    }]));
    let gateway = Arc::new(Gateway::new(Arc::clone(&pool), reqwest::Client::new()));

    let listener_port = free_port().await;
    let listener = GatewayListener::bind(listener_port).await.expect("gateway bind");
    let base = format!("http://127.0.0.1:{listener_port}");
    gateway.bindings.register(
        &Registration {
            org: "Capture".to_owned(),
            project: "forge".to_owned(),
            session: "capture-1".to_owned(),
            account: AccountKey("Capture".to_owned()),
            provider: Provider::Anthropic,
        },
        &base,
        &account_env,
    );
    let handler: Arc<dyn RouteHandler> = gateway.clone();
    tokio::spawn(async move { listener.run(handler).await });

    // Spawn the real CLI against the gateway, in a throwaway config dir
    // with the gateway's dummy credential: the request reaches the stub,
    // the stub 401s it, nothing is billed.
    let config_dir = tempfile::tempdir().expect("tempdir");
    let output = tokio::process::Command::new("claude")
        .arg("-p")
        .arg("Reply with the single word OK.")
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("ANTHROPIC_BASE_URL", format!("{base}/Capture/forge/capture-1"))
        .env("CLAUDE_CODE_OAUTH_TOKEN", "forge-gateway-unused")
        .env("ANTHROPIC_API_KEY", "")
        .output()
        .await
        .expect("claude runs");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let captured = loop {
        if let Some(body) = upstream.body.lock().clone() {
            break body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no request reached the stub; CLI output: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // Redact through the parsed value so only string contents change:
    // a byte-level pass ate numeric literals (12+ decimal digits ARE
    // hex digits) and corrupted the fixture outside every string.
    let mut value: serde_json::Value =
        serde_json::from_str(&captured).expect("the captured body parses as JSON");
    redact_string_values(&mut value, &config_dir.path().to_string_lossy());
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/messages_request.json");
    std::fs::write(&fixture, serde_json::to_string_pretty(&value).expect("serializes"))
        .expect("write fixture");
    println!("captured {} bytes -> {}", captured.len(), fixture.display());
}

async fn free_port() -> u16 {
    let probe = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("probe");
    probe.local_addr().expect("addr").port()
}
