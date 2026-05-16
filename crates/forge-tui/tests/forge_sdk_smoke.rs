//! End-to-end smoke tests: drive a real `claude` session through the
//! forge-sdk-backed `ForgeSdkBridge` worker.
//!
//! Marked `#[ignore]` because they need a real `claude` binary on PATH
//! and burn a small amount of API budget per run. Run manually with:
//!
//! ```
//! cargo test --test forge_sdk_smoke -- --ignored --nocapture
//! ```
//!
//! Coverage today:
//! - `forge_sdk_e2e_round_trip` — single prompt → one assistant chunk
//!   → `TurnComplete`. Validates the basic happy path.
//! - `forge_sdk_e2e_multi_turn` — two sequential prompts on the same
//!   session, validates session state survives between turns.
//! - `forge_sdk_e2e_tool_call_emits_event` — asks for a tool that is
//!   typically allow-listed in the developer's profile (Bash) so the
//!   `ToolCall` `SessionUpdate` fans out without a permission round-trip.
//!   Validates the `assistant->tool_use` translation path.
//!
//! Out of scope here (need manual TUI testing):
//! - `can_use_tool` round-trip with deny/allow choices. The CLI's
//!   auto-mode classifier and the developer's `settings.json` decide
//!   whether the callback fires; replicating that deterministically
//!   requires a `--settings` override per scenario, see
//!   `forge-test-harness/tests/sdk_scenarios_permission_deny.rs`.
//! - `AskUserQuestion`, MCP servers, slash commands, picker UI,
//!   `/resume`, status snapshot, model switching live in the TUI loop
//!   and need terminal-driven verification.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::manual_assert)]

use std::path::PathBuf;
use std::time::Duration;

use forge_workspace::{Agent, AgentEvent, SessionLaunchSettings};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Resolve the smoke-test `config_dir`. Honours `$CLAUDE_CONFIG_DIR`
/// (the same scheme the developer's profile uses) and falls back to
/// `$HOME/.claude` when unset — these tests are run manually against
/// the developer's real CLI install, so the natural fallback is the
/// developer's default profile.
fn smoke_config_dir() -> PathBuf {
    if let Ok(raw) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = raw.trim_end_matches('/');
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real `claude` binary on PATH; burns API budget"]
async fn forge_sdk_e2e_round_trip() {
    let agent_handle = Agent::spawn(smoke_config_dir(), None);
    let mut event_rx = agent_handle.take_events().expect("fresh handle has events");
    let agent: Arc<forge_workspace::AgentHandle> = Arc::new(agent_handle);

    // Kick off a session.
    agent
        .new_session(
            std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            SessionLaunchSettings::default(),
        )
        .expect("new_session queued");

    // Wait for Connected within 30s.
    let session_id = await_connected(&mut event_rx, Duration::from_secs(30)).await;
    eprintln!("e2e: connected to session {session_id}");

    // Send a tiny prompt.
    agent
        .prompt_text(session_id.clone(), "Reply with exactly the word OK.".to_owned())
        .expect("prompt_text queued");

    // Wait for the result frame within 60s and verify the assistant said something.
    let outcome = await_turn(&mut event_rx, Duration::from_secs(60)).await;
    assert!(outcome.saw_text, "expected at least one assistant text chunk before turn complete");

    // Tear down.
    drop(agent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real `claude` binary on PATH; burns API budget"]
async fn forge_sdk_e2e_multi_turn() {
    let agent_handle = Agent::spawn(smoke_config_dir(), None);
    let mut event_rx = agent_handle.take_events().expect("fresh handle has events");
    let agent: Arc<forge_workspace::AgentHandle> = Arc::new(agent_handle);

    agent
        .new_session(
            std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            SessionLaunchSettings::default(),
        )
        .expect("new_session queued");

    let session_id = await_connected(&mut event_rx, Duration::from_secs(30)).await;
    eprintln!("e2e multi_turn: connected to {session_id}");

    // Turn 1: establish a fact in the conversation context.
    agent
        .prompt_text(
            session_id.clone(),
            "Remember the codeword PUMPKIN. Just acknowledge.".to_owned(),
        )
        .expect("turn 1 queued");
    let turn1 = await_turn(&mut event_rx, Duration::from_secs(60)).await;
    assert!(turn1.saw_text, "turn 1 produced no assistant text");
    eprintln!("e2e multi_turn: turn 1 complete (text seen)");

    // Turn 2: probe whether the session retained turn 1's context. We
    // don't assert on the model's content (it might paraphrase) — we
    // only assert that another full turn round-trips without errors,
    // which proves the worker doesn't re-spawn the CLI between turns.
    agent
        .prompt_text(session_id, "What was the codeword? One word.".to_owned())
        .expect("turn 2 queued");
    let turn2 = await_turn(&mut event_rx, Duration::from_secs(60)).await;
    assert!(turn2.saw_text, "turn 2 produced no assistant text");
    eprintln!("e2e multi_turn: turn 2 complete (text seen)");

    drop(agent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real `claude` binary on PATH; burns API budget"]
async fn forge_sdk_e2e_tool_call_emits_event() {
    let agent_handle = Agent::spawn(smoke_config_dir(), None);
    let mut event_rx = agent_handle.take_events().expect("fresh handle has events");
    let agent: Arc<forge_workspace::AgentHandle> = Arc::new(agent_handle);

    agent
        .new_session(
            std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            SessionLaunchSettings::default(),
        )
        .expect("new_session queued");

    let session_id = await_connected(&mut event_rx, Duration::from_secs(30)).await;
    eprintln!("e2e tool_call: connected to {session_id}");

    // Bash is typically allow-listed in the developer's settings, so
    // the auto-mode classifier short-circuits the can_use_tool callback.
    // We only need to see a `ToolCall` SessionUpdate fan out from the
    // assistant message — the actual permission round-trip is covered
    // by `sdk_scenarios_permission_deny` in forge-test-harness.
    agent
        .prompt_text(
            session_id,
            "Use the Bash tool to run `echo OK_FROM_BASH` and report the output verbatim."
                .to_owned(),
        )
        .expect("prompt queued");

    let outcome = await_turn(&mut event_rx, Duration::from_secs(120)).await;
    assert!(
        outcome.saw_tool_call,
        "expected at least one ToolCall SessionUpdate during the turn (Bash tool)"
    );
    eprintln!("e2e tool_call: tool call observed");

    drop(agent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real `claude` binary on PATH; burns API budget"]
async fn forge_sdk_e2e_cancel_mid_turn() {
    let agent_handle = Agent::spawn(smoke_config_dir(), None);
    let mut event_rx = agent_handle.take_events().expect("fresh handle has events");
    let agent: Arc<forge_workspace::AgentHandle> = Arc::new(agent_handle);

    agent
        .new_session(
            std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            SessionLaunchSettings::default(),
        )
        .expect("new_session queued");
    let session_id = await_connected(&mut event_rx, Duration::from_secs(30)).await;
    eprintln!("e2e cancel: connected to {session_id}");

    // Kick off a turn likely to take a few seconds (writing a long
    // poem). We cancel before letting it finish — the worker should
    // route the interrupt to the CLI and emit either TurnComplete or
    // TurnError shortly after.
    agent
        .prompt_text(
            session_id.clone(),
            "Write a 500-word poem about Rust ownership semantics.".to_owned(),
        )
        .expect("prompt queued");

    // Wait for the first non-init event from the CLI before sending
    // cancel — a 2s fixed sleep races on loaded CI runners where the
    // turn hasn't started yet. We don't strictly require a chunk
    // (long tasks may emit only thinking chunks the translator
    // drops, or no chunk before the interrupt lands) — receiving
    // ANY event from the CLI after prompt is sufficient evidence
    // the turn is in flight. Bounded by 5s so a hung CLI still
    // fails cleanly rather than hanging the test.
    let pre_cancel_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < pre_cancel_deadline
        && tokio::time::timeout(Duration::from_millis(500), event_rx.recv())
            .await
            .is_err()
    {}
    agent.cancel(session_id).expect("cancel queued");
    eprintln!("e2e cancel: interrupt sent");

    // Drain until we see TurnComplete or TurnError.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut terminal = None;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            event_rx.recv(),
        )
        .await
        else {
            break;
        };
        if let AgentEvent::SdkMessage {
            msg: forge_primitives::Message::Result { is_error, subtype, .. },
            ..
        } = event
        {
            if !is_error && subtype == "success" {
                terminal = Some("complete");
            } else {
                eprintln!("e2e cancel: Result is_error={is_error} subtype={subtype}");
                terminal = Some("error");
            }
            break;
        }
    }
    assert!(terminal.is_some(), "no terminal turn frame after cancel");
    eprintln!("e2e cancel: turn finalized as {terminal:?}");

    drop(agent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real `claude` binary on PATH; burns API budget"]
async fn forge_sdk_e2e_status_and_context_snapshots() {
    let agent_handle = Agent::spawn(smoke_config_dir(), None);
    let mut event_rx = agent_handle.take_events().expect("fresh handle has events");
    let agent: Arc<forge_workspace::AgentHandle> = Arc::new(agent_handle);

    agent
        .new_session(
            std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            SessionLaunchSettings::default(),
        )
        .expect("new_session queued");
    let session_id = await_connected(&mut event_rx, Duration::from_secs(30)).await;
    eprintln!("e2e status: connected to {session_id}");

    // Drive a tiny prompt so the CLI's account info and context-usage
    // numbers are populated. account_info() returns None until at
    // least one stream-json frame mentions it.
    agent.prompt_text(session_id.clone(), "Reply with OK.".to_owned()).expect("prompt queued");
    let _ = await_turn(&mut event_rx, Duration::from_secs(60)).await;

    agent.get_status_snapshot(session_id.clone()).expect("status queued");
    agent.get_context_usage(session_id.clone()).expect("context queued");

    // Drain until we've seen both, with a generous timeout.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut saw_status = false;
    let mut saw_context = false;
    while tokio::time::Instant::now() < deadline && !(saw_status && saw_context) {
        let Ok(Some(event)) = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            event_rx.recv(),
        )
        .await
        else {
            break;
        };
        match event {
            AgentEvent::StatusSnapshot { .. } => saw_status = true,
            AgentEvent::ContextUsage { .. } => saw_context = true,
            _ => {}
        }
    }
    assert!(saw_status, "expected StatusSnapshot event");
    assert!(saw_context, "expected ContextUsage event");
    eprintln!("e2e status: both snapshots delivered");

    drop(agent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real `claude` binary on PATH; burns API budget"]
async fn forge_sdk_e2e_mcp_snapshot() {
    let agent_handle = Agent::spawn(smoke_config_dir(), None);
    let mut event_rx = agent_handle.take_events().expect("fresh handle has events");
    let agent: Arc<forge_workspace::AgentHandle> = Arc::new(agent_handle);

    agent
        .new_session(
            std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            SessionLaunchSettings::default(),
        )
        .expect("new_session queued");
    let session_id = await_connected(&mut event_rx, Duration::from_secs(30)).await;
    eprintln!("e2e mcp: connected to {session_id}");

    agent.get_mcp_snapshot(session_id).expect("mcp snapshot queued");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut got = false;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            event_rx.recv(),
        )
        .await
        else {
            break;
        };
        if let AgentEvent::McpSnapshot { servers, error, .. } = event {
            // The list may be empty (no MCP servers configured) — we
            // only care that the round-trip works without error.
            assert!(error.is_none(), "MCP snapshot error: {error:?}");
            eprintln!("e2e mcp: snapshot returned {} server(s)", servers.len());
            got = true;
            break;
        }
    }
    assert!(got, "expected McpSnapshot event");

    drop(agent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real `claude` binary on PATH; burns API budget"]
async fn forge_sdk_e2e_resume_session() {
    // Spawn a fresh session, drive one prompt, capture sid.
    let session_id = {
        let agent_handle = Agent::spawn(smoke_config_dir(), None);
        let mut event_rx = agent_handle.take_events().expect("fresh handle has events");
        let agent: Arc<forge_workspace::AgentHandle> = Arc::new(agent_handle);

        agent
            .new_session(
                std::env::current_dir().unwrap().to_string_lossy().into_owned(),
                SessionLaunchSettings::default(),
            )
            .expect("new_session queued");
        let sid = await_connected(&mut event_rx, Duration::from_secs(30)).await;
        eprintln!("e2e resume: fresh session {sid}");

        agent
            .prompt_text(sid.clone(), "Reply with the word PERSIST.".to_owned())
            .expect("first prompt queued");
        let _ = await_turn(&mut event_rx, Duration::from_secs(60)).await;

        // Tear down so the underlying CLI subprocess exits and its
        // session state lands on disk.
        drop(agent);
        sid
    };

    // Resume by id on a fresh worker.
    let agent_handle = Agent::spawn(smoke_config_dir(), None);
    let mut event_rx = agent_handle.take_events().expect("fresh handle has events");
    let agent: Arc<forge_workspace::AgentHandle> = Arc::new(agent_handle);

    agent
        .resume_session(
            session_id,
            std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            SessionLaunchSettings::default(),
        )
        .expect("resume_session queued");
    // The CLI may issue a brand-new session id when resuming; what we
    // care about is that we receive a Connected event without a
    // ConnectionFailed in between.
    let resumed_id = await_connected(&mut event_rx, Duration::from_secs(30)).await;
    eprintln!("e2e resume: phase 2 connected as {resumed_id}");

    drop(agent);
}

async fn await_connected(
    rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for Connected event");
        }
        let Ok(event) = tokio::time::timeout(remaining, rx.recv()).await else {
            panic!("timed out waiting for Connected event");
        };
        let Some(event) = event else {
            panic!("event channel closed before Connected");
        };
        match event {
            AgentEvent::Connected { session_id, .. } => return session_id,
            AgentEvent::ConnectionFailed { message } => {
                panic!("connection failed during smoke test: {message}");
            }
            other => {
                eprintln!("e2e: pre-connected event: {}", other.event_name());
            }
        }
    }
}

struct TurnOutcome {
    saw_text: bool,
    saw_tool_call: bool,
}

async fn await_turn(
    rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    timeout: Duration,
) -> TurnOutcome {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut outcome = TurnOutcome { saw_text: false, saw_tool_call: false };
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for TurnComplete");
        }
        let Ok(event) = tokio::time::timeout(remaining, rx.recv()).await else {
            panic!("timed out waiting for TurnComplete");
        };
        let Some(event) = event else {
            panic!("event channel closed before TurnComplete");
        };
        match event {
            AgentEvent::SdkMessage { msg, .. } => match msg {
                forge_primitives::Message::Assistant { message, .. } => {
                    let val = serde_json::to_value(&message).unwrap_or_default();
                    if let Some(blocks) = val.get("content").and_then(|c| c.as_array()) {
                        for b in blocks {
                            let t = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if t == "text" {
                                outcome.saw_text = true;
                            }
                            if t == "tool_use" || t == "server_tool_use" {
                                outcome.saw_tool_call = true;
                            }
                        }
                    }
                    eprintln!("e2e: assistant message");
                }
                forge_primitives::Message::Result { is_error, subtype, .. } => {
                    if !is_error && subtype == "success" {
                        return outcome;
                    }
                    panic!("turn errored: {subtype}");
                }
                _ => {}
            },
            other => eprintln!("e2e: event: {}", other.event_name()),
        }
    }
}
