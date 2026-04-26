//! M3 — multi-session + listing/mutations + mid-session control.
//!
//! Some tests redirect the SDK's projects-dir lookup by mutating
//! `$CLAUDE_CONFIG_DIR`. Rust 2024 made `std::env::set_var` `unsafe`,
//! so this crate's `Cargo.toml` down-grades the `unsafe_code` lint
//! from `forbid` to `deny` and the test file opts in below. Library
//! code in `forged` stays unsafe-free.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(unsafe_code)]

mod common {
    pub mod env_guard;
    pub mod env_lock;
}

use std::path::PathBuf;
use tempfile::TempDir;

use forge_daemon::methods::session::{SpawnResult, parse_spawn_params, spawn};
use forge_daemon::registry::DaemonState;
use forge_sdk::OptionsBuilder;

use crate::common::env_guard::EnvGuard;
use crate::common::env_lock::ENV_LOCK;

/// Mock that handles `initialize` + every subsequent `control_request`.
/// Used by the M3.6 / M3.7 round-trip tests (interrupt, `set_model`,
/// `mcp.toggle`, …) — `mock_claude.sh` only handles `initialize` and
/// would deadlock the actor on the first control reply.
const MOCK_CLAUDE_CONTROL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../forge-sdk/tests/fixtures/mock_claude_control.sh"
);

// =============================================================================
// M3.1 — full Options deserialiser
// =============================================================================

#[test]
fn parse_spawn_params_handles_full_options_shape() {
    let raw = serde_json::json!({
        "options": {
            "binary":          "/usr/local/bin/claude",
            "model":           "claude-opus-4-7",
            "permission_mode": "ask",
            "allowed_tools":   ["Read", "Edit", "Bash"],
            "skills":          ["all"],
            "max_turns":       10,
            "max_budget_usd":  0.50,
            "system_prompt":   { "kind": "preset", "append": null },
            "cwd":             "/tmp/forge-test",
            "resume":          null,
            "fork_session":    false,
            "continue_conversation": false,
            "include_partial_messages": true,
            "extra_args":      { "verbose": null },
            "env":             { "FOO": "bar" },
            "betas":           ["context-1m-2025-08-07"]
        }
    });
    let opts = parse_spawn_params(&raw).unwrap();
    assert_eq!(opts.options.binary, "/usr/local/bin/claude");
    assert_eq!(opts.options.model.as_deref(), Some("claude-opus-4-7"));
    assert_eq!(opts.options.allowed_tools, vec!["Read", "Edit", "Bash"]);
    assert_eq!(opts.options.skills, vec!["all"]);
    assert_eq!(opts.options.max_turns, Some(10));
    assert!((opts.options.max_budget_usd.unwrap() - 0.50).abs() < f64::EPSILON);
    assert_eq!(
        opts.options
            .cwd
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned()),
        Some("/tmp/forge-test".to_string())
    );
    assert!(opts.options.include_partial_messages);
    assert_eq!(opts.options.env.get("FOO").map(String::as_str), Some("bar"));
    assert_eq!(opts.options.betas, vec!["context-1m-2025-08-07"]);
    assert!(opts.options.extra_args.contains_key("verbose"));
}

#[test]
fn parse_spawn_params_minimal_only_binary() {
    let raw = serde_json::json!({ "options": { "binary": "claude" } });
    let opts = parse_spawn_params(&raw).unwrap();
    assert_eq!(opts.options.binary, "claude");
    assert_eq!(opts.options.model, None);
    assert!(opts.options.allowed_tools.is_empty());
}

#[test]
fn parse_spawn_params_empty_object_is_default() {
    let raw = serde_json::json!({});
    let opts = parse_spawn_params(&raw).unwrap();
    // Default binary is "claude" per OptionsBuilder.
    assert_eq!(opts.options.binary, "claude");
}

#[test]
fn parse_spawn_params_rejects_unknown_permission_mode() {
    let raw = serde_json::json!({
        "options": { "binary": "claude", "permission_mode": "shrug" }
    });
    let err = parse_spawn_params(&raw).unwrap_err();
    let s = err.to_string();
    assert!(
        s.contains("permission_mode") || s.contains("shrug"),
        "expected permission_mode mention; got: {s}"
    );
}

#[test]
fn parse_spawn_params_rejects_unknown_field() {
    let raw = serde_json::json!({
        "options": { "binary": "claude", "definitely_not_a_field": 42 }
    });
    let err = parse_spawn_params(&raw).unwrap_err();
    let s = err.to_string();
    assert!(
        s.contains("unknown field") || s.contains("definitely_not_a_field"),
        "expected unknown-field mention; got: {s}"
    );
}

#[test]
fn parse_spawn_params_handles_each_permission_mode_variant() {
    for variant in [
        "ask",
        "accept_edits",
        "plan",
        "bypass_permissions",
        "auto",
        "deny_permissions",
    ] {
        let raw = serde_json::json!({
            "options": { "binary": "claude", "permission_mode": variant }
        });
        parse_spawn_params(&raw).unwrap_or_else(|e| panic!("variant {variant}: {e}"));
    }
}

#[test]
fn parse_spawn_params_handles_thinking_variants() {
    for kind in ["adaptive", "disabled"] {
        let raw = serde_json::json!({
            "options": {
                "binary": "claude",
                "thinking": { "kind": kind }
            }
        });
        parse_spawn_params(&raw).unwrap_or_else(|e| panic!("kind {kind}: {e}"));
    }
    let raw = serde_json::json!({
        "options": {
            "binary": "claude",
            "thinking": { "kind": "enabled", "budget_tokens": 1024 }
        }
    });
    let opts = parse_spawn_params(&raw).unwrap();
    assert!(opts.options.thinking.is_some());
}

#[test]
fn parse_spawn_params_handles_system_prompt_inline_and_file() {
    let inline = serde_json::json!({
        "options": {
            "binary": "claude",
            "system_prompt": { "kind": "inline", "text": "say hi" }
        }
    });
    parse_spawn_params(&inline).unwrap();

    let file = serde_json::json!({
        "options": {
            "binary": "claude",
            "system_prompt": { "kind": "file", "path": "/tmp/sp.txt" }
        }
    });
    parse_spawn_params(&file).unwrap();
}

#[test]
fn parse_spawn_params_handles_plugins_and_add_dirs() {
    let raw = serde_json::json!({
        "options": {
            "binary": "claude",
            "add_dirs": ["/tmp/a", "/tmp/b"],
            "plugins": [{ "kind": "local", "path": "/tmp/plugin" }]
        }
    });
    let opts = parse_spawn_params(&raw).unwrap();
    assert_eq!(opts.options.add_dirs.len(), 2);
    assert_eq!(opts.options.plugins.len(), 1);
}

#[test]
fn parse_spawn_params_handles_effort_levels() {
    for level in ["low", "medium", "high", "max"] {
        let raw = serde_json::json!({
            "options": { "binary": "claude", "effort": level }
        });
        parse_spawn_params(&raw).unwrap_or_else(|e| panic!("level {level}: {e}"));
    }
    // Numeric effort.
    let raw = serde_json::json!({
        "options": { "binary": "claude", "effort": 7 }
    });
    let opts = parse_spawn_params(&raw).unwrap();
    assert!(opts.options.effort.is_some());
}

// =============================================================================
// M3.2 / M3.3 / M3.4 / M3.5 — sessions.* filesystem helpers
// =============================================================================

/// Seed a temp `$CLAUDE_CONFIG_DIR` with a project that holds N session
/// jsonl files.
///
/// Returns `(tmp, project_subdir, project_directory_path)` where the
/// `project_directory_path` is what callers pass as the `directory`
/// arg to the session helpers.
fn seed_projects(n: usize) -> (TempDir, PathBuf, String) {
    let tmp = TempDir::new().unwrap();

    // The `directory` argument is canonicalised by forge-sdk before
    // hashing, so we use a real existing path inside the tmp dir to
    // avoid `canonicalize` falling back. The tmp's own working subdir
    // is a real path on disk.
    let project_directory = tmp.path().join("workdir");
    std::fs::create_dir_all(&project_directory).unwrap();

    // Compute project key the same way the SDK does — via the public
    // `project_key_for_directory` helper.
    let project_key = forge_sdk::session::scan::project_key_for_directory(Some(
        project_directory.to_str().unwrap(),
    ));
    let project_subdir = tmp.path().join("projects").join(&project_key);
    std::fs::create_dir_all(&project_subdir).unwrap();

    for i in 0..n {
        // Use canonical UUIDs — forge-sdk validates the format.
        let sid = format!("00000000-0000-4000-8000-{i:012}");
        let path = project_subdir.join(format!("{sid}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "uuid": format!("uuid_{i}"),
            "sessionId": sid,
            "timestamp": format!("2026-04-22T00:00:0{i}.000Z"),
            "cwd": project_directory.to_str().unwrap(),
            "message": { "role": "user", "content": format!("hello {i}") }
        });
        std::fs::write(&path, format!("{line}\n")).unwrap();
    }

    let dir_str = project_directory.to_string_lossy().into_owned();
    (tmp, project_subdir, dir_str)
}

fn point_sdk_at(tmp: &TempDir) -> EnvGuard {
    EnvGuard::new("CLAUDE_CONFIG_DIR", tmp.path())
}

// Serialise tests that mutate $CLAUDE_CONFIG_DIR. Other tests don't
// touch the env so they can run in parallel. The lock itself lives in
// `common::env_lock` (round 3 — fix M10) so it's shared with other
// test files (`m6_operations.rs`) that also mutate env state — without
// a shared lock, tests across files could race.

#[test]
fn sessions_list_returns_seeded_entries() {
    let _g = ENV_LOCK.lock();
    let (tmp, _projects_dir, project_dir) = seed_projects(3);
    let _guard = point_sdk_at(&tmp);
    let result = forge_daemon::methods::sessions::list(Some(project_dir), Some(10), 0).unwrap();
    assert_eq!(result.sessions.len(), 3, "expected 3 seeded sessions");
}

#[test]
fn sessions_list_honours_limit_and_offset() {
    let _g = ENV_LOCK.lock();
    let (tmp, _, project_dir) = seed_projects(5);
    let _guard = point_sdk_at(&tmp);
    let r = forge_daemon::methods::sessions::list(Some(project_dir), Some(2), 1).unwrap();
    assert_eq!(r.sessions.len(), 2);
}

#[test]
fn sessions_info_returns_some_for_known_id() {
    let _g = ENV_LOCK.lock();
    let (tmp, _, project_dir) = seed_projects(1);
    let _guard = point_sdk_at(&tmp);
    let sid = "00000000-0000-4000-8000-000000000000".to_string();
    let info = forge_daemon::methods::sessions::info(sid.clone().into(), Some(project_dir))
        .unwrap()
        .info;
    assert!(info.is_some(), "expected Some(SDKSessionInfo)");
    assert_eq!(info.unwrap().session_id, sid);
}

#[test]
fn sessions_info_returns_none_for_unknown_id() {
    let _g = ENV_LOCK.lock();
    let (tmp, _, project_dir) = seed_projects(0);
    let _guard = point_sdk_at(&tmp);
    let info = forge_daemon::methods::sessions::info(
        "00000000-0000-4000-8000-deadbeefface".into(),
        Some(project_dir),
    )
    .unwrap()
    .info;
    assert!(info.is_none());
}

#[test]
fn sessions_messages_returns_full_transcript_with_watermark() {
    let _g = ENV_LOCK.lock();
    let (tmp, dir, project_dir) = seed_projects(0);
    let sid = "00000000-0000-4000-8000-aaaaaaaaaaaa";
    let path = dir.join(format!("{sid}.jsonl"));
    let lines = [
        serde_json::json!({
            "type": "user", "uuid": "msg_1", "sessionId": sid,
            "message": { "role": "user", "content": [{"type":"text","text":"hi"}] }
        }),
        serde_json::json!({
            "type": "assistant", "uuid": "msg_2", "sessionId": sid,
            "message": {
                "id": "msg_xyz", "role": "assistant", "model": "claude",
                "content": [{"type":"text","text":"hello"}]
            }
        }),
    ];
    let body = lines.iter().fold(String::new(), |mut acc, v| {
        use std::fmt::Write;
        let _ = writeln!(&mut acc, "{v}");
        acc
    });
    std::fs::write(&path, body).unwrap();

    let _guard = point_sdk_at(&tmp);
    let r = forge_daemon::methods::sessions::messages(sid.into(), Some(project_dir)).unwrap();
    assert_eq!(r.messages.len(), 2);
    assert_eq!(
        r.watermark.as_deref(),
        Some("msg_2"),
        "watermark should be the highest uuid in the transcript"
    );
}

#[test]
fn sessions_messages_empty_transcript_returns_none_watermark() {
    let _g = ENV_LOCK.lock();
    let (tmp, _, project_dir) = seed_projects(0);
    let _guard = point_sdk_at(&tmp);
    let r = forge_daemon::methods::sessions::messages(
        "00000000-0000-4000-8000-bbbbbbbbbbbb".into(),
        Some(project_dir),
    )
    .unwrap();
    assert!(r.messages.is_empty());
    assert!(r.watermark.is_none());
}

#[test]
fn sessions_list_subagents_returns_empty_for_session_with_no_subagents() {
    let _g = ENV_LOCK.lock();
    let (tmp, _, project_dir) = seed_projects(1);
    let _guard = point_sdk_at(&tmp);
    let r = forge_daemon::methods::sessions::list_subagents(
        "00000000-0000-4000-8000-000000000000".into(),
        Some(project_dir),
    )
    .unwrap();
    assert!(r.subagent_ids.is_empty());
}

#[test]
fn sessions_subagent_messages_empty_for_unknown_subagent() {
    let _g = ENV_LOCK.lock();
    let (tmp, _, project_dir) = seed_projects(1);
    let _guard = point_sdk_at(&tmp);
    let r = forge_daemon::methods::sessions::subagent_messages(
        "00000000-0000-4000-8000-000000000000".into(),
        "sub_unknown".into(),
        Some(project_dir),
    )
    .unwrap();
    assert!(r.messages.is_empty());
}

#[test]
fn sessions_project_key_matches_sdk_output() {
    let path = "/Users/vedhavyas/Projects/forge";
    let key = forge_daemon::methods::sessions::project_key(Some(path.into())).unwrap();
    let expected = forge_sdk::session::scan::project_key_for_directory(Some(path));
    assert_eq!(key.project_key, expected);
}

#[test]
fn sessions_project_key_none_uses_cwd() {
    let key = forge_daemon::methods::sessions::project_key(None).unwrap();
    let expected = forge_sdk::session::scan::project_key_for_directory(None);
    assert_eq!(key.project_key, expected);
}

// ---- Mutations ------------------------------------------------------------

#[test]
fn sessions_rename_writes_custom_title() {
    let _g = ENV_LOCK.lock();
    let (tmp, _, project_dir) = seed_projects(1);
    let _guard = point_sdk_at(&tmp);
    let sid = "00000000-0000-4000-8000-000000000000".to_string();
    forge_daemon::methods::sessions::rename(
        sid.clone().into(),
        "renamed-title".into(),
        Some(project_dir.clone()),
    )
    .unwrap();
    let info = forge_daemon::methods::sessions::info(sid.into(), Some(project_dir))
        .unwrap()
        .info
        .unwrap();
    assert_eq!(info.custom_title.as_deref(), Some("renamed-title"));
}

#[test]
fn sessions_tag_sets_then_clears() {
    let _g = ENV_LOCK.lock();
    let (tmp, _, project_dir) = seed_projects(1);
    let _guard = point_sdk_at(&tmp);
    let sid = "00000000-0000-4000-8000-000000000000".to_string();

    forge_daemon::methods::sessions::tag(
        sid.clone().into(),
        Some("design".into()),
        Some(project_dir.clone()),
    )
    .unwrap();
    let info = forge_daemon::methods::sessions::info(sid.clone().into(), Some(project_dir.clone()))
        .unwrap()
        .info
        .unwrap();
    assert_eq!(info.tag.as_deref(), Some("design"));

    forge_daemon::methods::sessions::tag(sid.clone().into(), None, Some(project_dir.clone()))
        .unwrap();
    let info = forge_daemon::methods::sessions::info(sid.into(), Some(project_dir))
        .unwrap()
        .info
        .unwrap();
    assert_eq!(info.tag, None);
}

#[test]
fn sessions_delete_removes_jsonl() {
    let _g = ENV_LOCK.lock();
    let (tmp, dir, project_dir) = seed_projects(1);
    let sid = "00000000-0000-4000-8000-000000000000".to_string();
    let path = dir.join(format!("{sid}.jsonl"));
    assert!(path.exists());

    let _guard = point_sdk_at(&tmp);
    forge_daemon::methods::sessions::delete(sid.into(), Some(project_dir)).unwrap();
    assert!(!path.exists());
}

#[test]
fn sessions_fork_creates_a_new_session_with_copied_entries() {
    let _g = ENV_LOCK.lock();
    let (tmp, dir, project_dir) = seed_projects(0);
    let sid = "00000000-0000-4000-8000-cccccccccccc";
    let path = dir.join(format!("{sid}.jsonl"));
    let lines = [
        serde_json::json!({"type": "user", "uuid": "11111111-1111-4111-8111-111111111111", "sessionId": sid,
            "message": {"role":"user","content":[{"type":"text","text":"a"}]}}),
        serde_json::json!({"type": "user", "uuid": "22222222-2222-4222-8222-222222222222", "sessionId": sid,
            "message": {"role":"user","content":[{"type":"text","text":"b"}]}}),
        serde_json::json!({"type": "user", "uuid": "33333333-3333-4333-8333-333333333333", "sessionId": sid,
            "message": {"role":"user","content":[{"type":"text","text":"c"}]}}),
    ];
    let body = lines.iter().fold(String::new(), |mut acc, v| {
        use std::fmt::Write;
        let _ = writeln!(&mut acc, "{v}");
        acc
    });
    std::fs::write(&path, body).unwrap();

    let _guard = point_sdk_at(&tmp);
    let result = forge_daemon::methods::sessions::fork(
        sid.into(),
        Some("22222222-2222-4222-8222-222222222222".into()),
        None,
        Some(project_dir),
    )
    .unwrap();
    assert!(
        !result.session_id.is_empty(),
        "fork should mint a new session id"
    );
}

// =============================================================================
// M3.6 — mid-session control (interrupt / set_permission_mode / set_model /
// rewind_files / stop_task)
// =============================================================================

/// Spawn a session backed by `mock_claude_control.sh` which responds
/// to every `control_request` — required for any M3.6 / M3.7
/// round-trip test. The plain `mock_claude.sh` only answers
/// `initialize`.
async fn spawn_control_mock_session(state: &DaemonState) -> forge_daemon::session_state::SessionId {
    let opts = OptionsBuilder::new().binary(MOCK_CLAUDE_CONTROL).build();
    let SpawnResult { session_id, .. } = spawn(state, opts).await.unwrap();
    session_id
}

#[tokio::test]
async fn session_interrupt_proxies_to_client() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let res = forge_daemon::methods::session::interrupt(&state, &session_id).await;
    assert!(res.is_ok(), "interrupt: {res:?}");
}

#[tokio::test]
async fn session_interrupt_returns_session_not_found_for_unknown() {
    let state = DaemonState::new();
    let unknown = forge_daemon::session_state::SessionId("sess_bogus".into());
    let err = forge_daemon::methods::session::interrupt(&state, &unknown)
        .await
        .unwrap_err();
    assert!(matches!(err, forge_daemon::Error::SessionNotFound(_)));
}

#[tokio::test]
async fn session_set_permission_mode_proxies_to_client() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let res = forge_daemon::methods::session::set_permission_mode(
        &state,
        &session_id,
        forge_sdk::PermissionMode::Auto,
    )
    .await;
    assert!(res.is_ok(), "set_permission_mode: {res:?}");
}

#[tokio::test]
async fn session_set_model_proxies_to_client() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let res = forge_daemon::methods::session::set_model(
        &state,
        &session_id,
        Some("claude-opus-4-7".into()),
    )
    .await;
    assert!(res.is_ok(), "set_model: {res:?}");
}

#[tokio::test]
async fn session_set_model_with_none_reverts_default() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let res = forge_daemon::methods::session::set_model(&state, &session_id, None).await;
    assert!(res.is_ok(), "set_model(None): {res:?}");
}

#[tokio::test]
async fn session_rewind_files_proxies_to_client() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let res =
        forge_daemon::methods::session::rewind_files(&state, &session_id, "msg_test".into()).await;
    assert!(res.is_ok(), "rewind_files: {res:?}");
}

#[tokio::test]
async fn session_stop_task_proxies_to_client() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let res =
        forge_daemon::methods::session::stop_task(&state, &session_id, "task_test".into()).await;
    assert!(res.is_ok(), "stop_task: {res:?}");
}

// =============================================================================
// M3.7 — MCP + context handlers
// =============================================================================
//
// Round 3 — fix M8 (partial). The mock now supports an opt-in
// `FORGED_MOCK_ECHO_SUBTYPE` env var that echoes every observed
// control_request subtype to a temp file. The `*_dispatches_subtype`
// strengthening tests below read that file post-call to assert the
// right `Client::*` method fired — no longer just "the actor stayed
// alive".
//
// The original M3.6/M3.7 round-trip tests (interrupt, set_model,
// rewind_files, stop_task, mcp.status, context.get) still keep their
// "no SessionNotFound / no actor-gone" shape — the M8 echo path adds
// a stronger sibling test rather than rewriting every existing one.
// Future work: port every round-trip test to use the echo, deleting
// the weaker variants.

/// Read the contents of the subtype-echo file as a Vec<String> of
/// non-empty lines. Helper for the strengthening tests below.
fn read_echo_file(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn session_interrupt_dispatches_subtype_via_echo() {
    let _g = ENV_LOCK.lock_async().await;
    let tmp = TempDir::new().unwrap();
    let echo_path = tmp.path().join("subtypes.log");
    let _echo_guard = EnvGuard::new("FORGED_MOCK_ECHO_SUBTYPE", &echo_path);

    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    forge_daemon::methods::session::interrupt(&state, &session_id)
        .await
        .unwrap();

    // Give the mock a beat to flush the line — control requests
    // round-trip via two mpsc channels and a subprocess.
    for _ in 0..50 {
        let lines = read_echo_file(&echo_path);
        if lines.iter().any(|l| l == "interrupt") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let lines = read_echo_file(&echo_path);
    panic!("expected 'interrupt' subtype echoed; got {lines:?}");
}

#[tokio::test]
async fn mcp_status_dispatches_subtype_via_echo() {
    let _g = ENV_LOCK.lock_async().await;
    let tmp = TempDir::new().unwrap();
    let echo_path = tmp.path().join("subtypes.log");
    let _echo_guard = EnvGuard::new("FORGED_MOCK_ECHO_SUBTYPE", &echo_path);

    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    // The status response shape is wrong for McpStatusResponse — the
    // mock returns `{"servers":[]}` and the SDK errors out on parse.
    // Test contract: the SUBTYPE echoed is `mcp_status`, regardless
    // of how the SDK then surfaces the parse error.
    let _ = forge_daemon::methods::mcp::status(&state, &session_id).await;

    for _ in 0..50 {
        let lines = read_echo_file(&echo_path);
        if lines.iter().any(|l| l == "mcp_status") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let lines = read_echo_file(&echo_path);
    panic!("expected 'mcp_status' subtype echoed; got {lines:?}");
}

#[tokio::test]
async fn context_get_dispatches_subtype_via_echo() {
    let _g = ENV_LOCK.lock_async().await;
    let tmp = TempDir::new().unwrap();
    let echo_path = tmp.path().join("subtypes.log");
    let _echo_guard = EnvGuard::new("FORGED_MOCK_ECHO_SUBTYPE", &echo_path);

    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let _ = forge_daemon::methods::context::get(&state, &session_id).await;

    for _ in 0..50 {
        let lines = read_echo_file(&echo_path);
        if lines.iter().any(|l| l == "get_context_usage") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let lines = read_echo_file(&echo_path);
    panic!("expected 'get_context_usage' subtype echoed; got {lines:?}");
}

#[tokio::test]
async fn mcp_status_proxies_through_actor() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    // mock_claude_control.sh replies `{"servers":[]}` — wrong shape vs
    // McpStatusResponse — so the SDK returns a `MessageParse` Sdk
    // error. The contract under test is that the call reaches the
    // actor (no SessionNotFound, no "actor gone" InternalError).
    let res = forge_daemon::methods::mcp::status(&state, &session_id).await;
    if let Err(e) = &res {
        assert!(
            !matches!(e, forge_daemon::Error::SessionNotFound(_)),
            "must not be SessionNotFound: {e:?}"
        );
        let s = e.to_string();
        assert!(
            !s.contains("actor gone") && !s.contains("dropped reply"),
            "actor must not have died: {e:?}"
        );
    }
}

#[tokio::test]
async fn mcp_status_returns_session_not_found_for_unknown() {
    let state = DaemonState::new();
    let unknown = forge_daemon::session_state::SessionId("sess_bogus".into());
    let err = forge_daemon::methods::mcp::status(&state, &unknown)
        .await
        .unwrap_err();
    assert!(matches!(err, forge_daemon::Error::SessionNotFound(_)));
}

#[tokio::test]
async fn mcp_reconnect_proxies_to_client() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let res = forge_daemon::methods::mcp::reconnect(&state, &session_id, "some_server").await;
    assert!(res.is_ok(), "mcp_reconnect: {res:?}");
}

#[tokio::test]
async fn mcp_toggle_proxies_to_client() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    let res = forge_daemon::methods::mcp::toggle(&state, &session_id, "some_server", true).await;
    assert!(res.is_ok(), "mcp_toggle: {res:?}");
}

#[tokio::test]
async fn context_get_proxies_through_actor() {
    let state = DaemonState::new();
    let session_id = spawn_control_mock_session(&state).await;
    // Mock returns `{"used":0,"budget":200000}` — wrong shape vs
    // ContextUsageResponse. Test contract: the dispatch path reaches
    // the actor. Either Ok or Sdk parse error is fine.
    let res = forge_daemon::methods::context::get(&state, &session_id).await;
    if let Err(e) = &res {
        assert!(
            !matches!(e, forge_daemon::Error::SessionNotFound(_)),
            "must not be SessionNotFound: {e:?}"
        );
        let s = e.to_string();
        assert!(
            !s.contains("actor gone") && !s.contains("dropped reply"),
            "actor must not have died: {e:?}"
        );
    }
}

#[tokio::test]
async fn context_get_returns_session_not_found_for_unknown() {
    let state = DaemonState::new();
    let unknown = forge_daemon::session_state::SessionId("sess_bogus".into());
    let err = forge_daemon::methods::context::get(&state, &unknown)
        .await
        .unwrap_err();
    assert!(matches!(err, forge_daemon::Error::SessionNotFound(_)));
}
