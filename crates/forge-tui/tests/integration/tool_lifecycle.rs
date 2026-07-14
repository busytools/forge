// =====
// TESTS: 21
// =====
//
// Tool call lifecycle integration tests.
// Validates the full create -> update -> complete flow for tool calls
// over the wire-message dispatch path.

use forge_tui::agent::model;
use forge_tui::app::session::UiSession;
use forge_tui::app::{
    App, AppStatus, BackgroundTask, BlockCache, ChatMessage, MessageBlock, MessageRole,
    TerminalSnapshotMode, ToolCallInfo, ToolCallScope,
};
use pretty_assertions::assert_eq;

use crate::helpers::{active_session_key, send_client_event, test_app};
use crate::message_helpers::{
    assistant_message, assistant_message_with_parent, result_success_message, send_msg, text_block,
    tool_result_block, tool_result_error_block, tool_use_block, user_message,
};
use forge_workspace::{SessionKey, SessionUpdate};

fn tool_call_block<'a>(app: &'a App, id: &str) -> &'a ToolCallInfo {
    let (message_index, block_index) = app.lookup_tool_call(id).expect("missing tool index");
    app.messages()
        .get(message_index)
        .and_then(|message| message.blocks.get(block_index))
        .and_then(|block| match block {
            MessageBlock::ToolCall(tool_call) => Some(tool_call.as_ref()),
            _ => None,
        })
        .expect("expected ToolCall block")
}

// --- ToolCallUpdate lifecycle ---

#[tokio::test]
async fn tool_call_updates_apply_terminal_statuses_and_title_fields() {
    let mut app = test_app();
    app.status = AppStatus::Running;

    // Tool_use carries the file_path that drives `tool_title("Read", input)`
    // → "Read src/lib.rs". Wire path sets the title at creation time;
    // completion via tool_result transitions status to Completed.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-update",
            "Read",
            serde_json::json!({"file_path": "src/lib.rs"}),
        )]),
    );
    send_msg(&mut app, user_message(vec![tool_result_block("tc-update", serde_json::json!("ok"))]));

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-fail",
            "Write",
            serde_json::json!({"file_path": "out.txt", "content": "x"}),
        )]),
    );
    // is_error=true in tool_result drives status→Failed via build_tool_result_fields.
    send_msg(
        &mut app,
        user_message(vec![tool_result_error_block("tc-fail", serde_json::json!("bang"))]),
    );

    let updated = tool_call_block(&app, "tc-update");
    assert_eq!(updated.title, "Read src/lib.rs");
    assert!(matches!(updated.status, model::ToolCallStatus::Completed));

    let failed = tool_call_block(&app, "tc-fail");
    assert!(matches!(failed.status, model::ToolCallStatus::Failed));
}

// --- All tools terminal -> Thinking ---

#[tokio::test]
async fn terminal_tool_statuses_transition_running_to_thinking_once_all_calls_finish() {
    let mut app = test_app();
    app.status = AppStatus::Running;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-a",
            "Read",
            serde_json::json!({"file_path": "a.txt"}),
        )]),
    );
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-b",
            "Read",
            serde_json::json!({"file_path": "b.txt"}),
        )]),
    );

    assert!(matches!(app.status, AppStatus::Running));

    send_msg(&mut app, user_message(vec![tool_result_block("tc-a", serde_json::json!("ok"))]));
    assert!(matches!(app.status, AppStatus::Running), "one still in progress");

    send_msg(&mut app, user_message(vec![tool_result_block("tc-b", serde_json::json!("ok"))]));
    assert!(matches!(app.status, AppStatus::Thinking), "all-complete should resume thinking");

    let mut mixed_app = test_app();
    mixed_app.status = AppStatus::Running;

    send_msg(
        &mut mixed_app,
        assistant_message(vec![tool_use_block(
            "tc-x",
            "Read",
            serde_json::json!({"file_path": "x.txt"}),
        )]),
    );
    send_msg(
        &mut mixed_app,
        assistant_message(vec![tool_use_block(
            "tc-y",
            "Read",
            serde_json::json!({"file_path": "y.txt"}),
        )]),
    );

    send_msg(
        &mut mixed_app,
        user_message(vec![tool_result_block("tc-x", serde_json::json!("ok"))]),
    );
    send_msg(
        &mut mixed_app,
        user_message(vec![tool_result_error_block("tc-y", serde_json::json!("bang"))]),
    );

    assert!(
        matches!(mixed_app.status, AppStatus::Thinking),
        "mixed terminal outcomes should also resume thinking"
    );
}

// --- Task tool call tracking ---

#[tokio::test]
async fn task_tool_calls_leave_active_set_only_on_terminal_statuses() {
    let mut app = test_app();

    // "Task" tool name normalises to ToolKind::Think and registers
    // the call as a Task in app.active_task_ids.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "task-pend",
            "Task",
            serde_json::json!({"description": "Running subtask"}),
        )]),
    );
    assert!(app.active_task_ids().contains("task-pend"), "new Task should be tracked");

    // The wire path has no equivalent of the SessionUpdate-only
    // intermediate "Pending" status; resending an open tool_use keeps
    // status=in_progress, which preserves active-task tracking.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "task-pend",
            "Task",
            serde_json::json!({"description": "Running subtask"}),
        )]),
    );
    assert!(app.active_task_ids().contains("task-pend"), "still in-progress should stay active");

    send_msg(&mut app, user_message(vec![tool_result_block("task-pend", serde_json::json!("ok"))]));
    assert!(!app.active_task_ids().contains("task-pend"), "completed Task should be removed");

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "task-fail",
            "Task",
            serde_json::json!({"description": "Subtask"}),
        )]),
    );
    assert!(app.active_task_ids().contains("task-fail"));

    send_msg(
        &mut app,
        user_message(vec![tool_result_error_block("task-fail", serde_json::json!("bang"))]),
    );
    assert!(!app.active_task_ids().contains("task-fail"), "failed Task should also be removed");
}

#[tokio::test]
async fn subagent_child_tools_use_explicit_parent_linkage_only() {
    let mut app = test_app();

    // Root Task tool, no parent linkage.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "task-root",
            "Task",
            serde_json::json!({"description": "Research"}),
        )]),
    );

    // Child Bash tool - parent_tool_use_id carried at the assistant
    // envelope level (the wire shape for sub-agent child tools).
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block(
                "child-bash",
                "Bash",
                serde_json::json!({"command": "echo child"}),
            )],
            "task-root",
        ),
    );

    // Main-agent Bash tool, no envelope-level parent.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "main-bash",
            "Bash",
            serde_json::json!({"command": "echo main"}),
        )]),
    );

    assert_eq!(app.tool_call_scope("task-root"), Some(ToolCallScope::SubagentRoot));
    assert_eq!(
        app.tool_call_scope("child-bash"),
        Some(ToolCallScope::SubagentChild { parent_tool_use_id: "task-root".to_owned() })
    );
    assert_eq!(app.tool_call_scope("main-bash"), Some(ToolCallScope::MainAgent));
    assert!(tool_call_block(&app, "child-bash").hidden);
    assert!(!tool_call_block(&app, "main-bash").hidden);
}

#[tokio::test]
async fn tool_call_update_parent_linkage_marks_existing_tool_hidden() {
    let mut app = test_app();

    // Initial Bash tool with no parent linkage.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "child-late",
            "Bash",
            serde_json::json!({"command": "echo child"}),
        )]),
    );
    assert!(!tool_call_block(&app, "child-late").hidden);

    // A subsequent tool_use with the same id and an envelope-level
    // parent re-runs apply_tool_use_block which re-passes meta
    // (now containing parentToolUseId) through the update path.
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block(
                "child-late",
                "Bash",
                serde_json::json!({"command": "echo child"}),
            )],
            "task-root",
        ),
    );

    assert_eq!(
        app.tool_call_scope("child-late"),
        Some(ToolCallScope::SubagentChild { parent_tool_use_id: "task-root".to_owned() })
    );
    assert!(tool_call_block(&app, "child-late").hidden);
}

// --- Collapsed tool calls ---

#[tokio::test]
async fn session_collapse_preference_stays_stable_across_tool_call_lifecycle() {
    let mut app = test_app();
    app.tools_collapsed = true;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-col",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );
    assert!(app.tools_collapsed, "session preference should remain collapsed");
    assert!(matches!(tool_call_block(&app, "tc-col").status, model::ToolCallStatus::InProgress));

    // A duplicate tool_use re-fires apply_tool_use_block, which
    // forces status back to in_progress for already-open calls.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-col",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );
    assert!(app.tools_collapsed, "in-progress updates should not flip the preference");
    assert!(matches!(tool_call_block(&app, "tc-col").status, model::ToolCallStatus::InProgress));

    send_msg(&mut app, user_message(vec![tool_result_block("tc-col", serde_json::json!("ok"))]));
    assert!(app.tools_collapsed, "completed updates should keep the preference");
    assert!(matches!(tool_call_block(&app, "tc-col").status, model::ToolCallStatus::Completed));

    let mut expanded_app = test_app();
    expanded_app.tools_collapsed = false;

    send_msg(
        &mut expanded_app,
        assistant_message(vec![tool_use_block(
            "tc-exp",
            "Write",
            serde_json::json!({"file_path": "file"}),
        )]),
    );
    assert!(!expanded_app.tools_collapsed, "expanded preference should remain expanded");

    send_msg(
        &mut expanded_app,
        user_message(vec![tool_result_block("tc-exp", serde_json::json!("ok"))]),
    );
    assert!(!expanded_app.tools_collapsed);
    assert!(matches!(
        tool_call_block(&expanded_app, "tc-exp").status,
        model::ToolCallStatus::Completed
    ));
}

// --- Multiple tool calls indexed correctly ---

#[tokio::test]
async fn multiple_tool_calls_independently_indexed() {
    let mut app = test_app();

    for i in 0..5 {
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                &format!("tc-{i}"),
                "Read",
                serde_json::json!({"file_path": format!("file-{i}")}),
            )]),
        );
    }

    assert_eq!(app.tool_call_index().len(), 5);
    for i in 0..5 {
        let key = format!("tc-{i}");
        assert!(app.tool_call_index().contains_key(&key), "missing {key}");
    }
}

// --- Edge cases: tool call update propagation ---

#[tokio::test]
async fn tool_call_update_via_meta_sets_sdk_tool_name() {
    let mut app = test_app();

    // Initial tool_use with `name="WebSearch"` already produces
    // sdk_tool_name="WebSearch" via meta.claudeCode.toolName at
    // creation time on the wire path; no separate "meta update"
    // event is required.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-meta",
            "WebSearch",
            serde_json::json!({"query": "rust"}),
        )]),
    );

    let (mi, bi) = app.lookup_tool_call("tc-meta").expect("missing tool index");
    if let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] {
        assert_eq!(tc.sdk_tool_name, "WebSearch");
    } else {
        panic!("expected ToolCall block");
    }
}

#[tokio::test]
async fn title_shortened_relative_to_cwd() {
    let mut app = test_app();
    app.set_cwd_raw("/home/user/project");

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-shorten",
            "Read",
            serde_json::json!({"file_path": "/home/user/project/src/main.rs"}),
        )]),
    );

    let (mi, bi) = app.lookup_tool_call("tc-shorten").expect("missing tool index");
    if let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] {
        assert_eq!(tc.title, "Read src/main.rs", "absolute path shortened to relative");
    } else {
        panic!("expected ToolCall block");
    }
}

/// Real-wire end-to-end contract for the SUBAGENTS Inspector
/// section. Drives the production
/// `apply_tool_use_block` -> `handle_tool_call` ->
/// `register_tool_call_scope` pipeline (NOT the synthetic
/// `App::register_tool_call_scope` shortcut some unit tests use) for
/// an Agent dispatch + a SubagentChild, then asserts:
///
/// 1. Scope registration: root scopes `SubagentRoot`, child scopes
///    `SubagentChild { parent }`.
/// 2. Derive: `App::subagents_view` surfaces one entry with the
///    expected tail while the root is still running.
/// 3. Inspector-only: the root + child are BOTH chat-suppressed
///    (`hidden: true`) so they never reach the chat renderer.
#[tokio::test]
async fn subagent_root_via_real_wire_surfaces_in_subagents_view() {
    let mut app = test_app();

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "map hidden tool calls",
                "prompt": "map hidden tool calls",
            }),
        )]),
    );
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block(
                "toolu_child",
                "Grep",
                serde_json::json!({"pattern": "SubagentChild"}),
            )],
            "toolu_root",
        ),
    );

    assert_eq!(app.tool_call_scope("toolu_root"), Some(ToolCallScope::SubagentRoot));
    assert_eq!(
        app.tool_call_scope("toolu_child"),
        Some(ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() }),
    );

    let view = app.subagents_view();
    assert_eq!(
        view.len(),
        1,
        "real-wire Agent dispatch must surface in subagents_view; got {view:?}",
    );
    let entry = &view[0];
    assert_eq!(entry.tool_use_id, "toolu_root");
    assert_eq!(entry.total_count, 1);
    assert_eq!(entry.tail.len(), 1);
    assert_eq!(entry.tail[0].sdk_tool_name, "Grep");

    // Inspector-only: the SubagentRoot itself is chat-suppressed - no
    // card, no group. The hidden flag stays in the message block (so
    // `subagents_view` still sees it) but the chat render skips it.
    assert!(
        tool_call_block(&app, "toolu_root").hidden,
        "SubagentRoot must be hidden from chat (Inspector-only); got hidden={}",
        tool_call_block(&app, "toolu_root").hidden,
    );
    assert!(tool_call_block(&app, "toolu_child").hidden, "SubagentChild stays hidden as before");
}

/// Drain-to-hidden: a subagent root stays visible while its status is
/// in-flight, then the section clears once the terminal `task_updated`
/// flips the root card terminal. Guards against a regression that stopped
/// applying the terminal status, which would leave a completed subagent
/// stuck-visible. Mirrors the MONITORS
/// `two_monitors_completing_in_order_clears_section` contract, driven
/// over the real wire path.
#[tokio::test]
async fn subagent_section_clears_when_terminal_task_updated_flips_root() {
    let mut app = test_app();

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "long-running background scan",
                "prompt": "long-running background scan",
            }),
        )]),
    );
    // task_started maps the task_id to the root.
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "long-running background scan".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("Explore".to_owned()),
        },
    );
    assert_eq!(
        app.subagents_view().len(),
        1,
        "backgrounded subagent is visible while its root is in-flight; got {:?}",
        app.subagents_view(),
    );

    // Terminal task_updated flips the root card terminal.
    send_msg(
        &mut app,
        forge_primitives::Message::TaskUpdated {
            task_id: "task-root".to_owned(),
            patch: forge_primitives::messages::TaskUpdatePatch {
                status: Some("completed".to_owned()),
                end_time: None,
            },
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    assert!(
        app.subagents_view().is_empty(),
        "section clears once the terminal task_updated flips the root terminal; got {:?}",
        app.subagents_view(),
    );
}

/// Producer-path coverage: the session-scoped task map is populated by the
/// REAL `handle_task_started`, not a test setter. A backgrounded agent
/// whose spawning turn finalises (turn_state wiped) must still surface in
/// SUBAGENTS via the session-map INTERSECT `background_tasks` registry. An
/// arg-swap in `handle_task_started` (storing tool_use_id -> task_id)
/// silently breaks this while the hand-populated unit tests stay green, so
/// this drives the real wire path end to end.
#[tokio::test]
async fn backgrounded_agent_survives_turn_reset_over_real_wire_path() {
    let mut app = test_app();

    // Agent dispatch + one child tool call (the live tail).
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "long-running background agent",
                "prompt": "long-running background agent",
            }),
        )]),
    );
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block(
                "toolu_child",
                "Read",
                serde_json::json!({"file": "conv-row.tsx"}),
            )],
            "toolu_root",
        ),
    );
    // Real producer: TaskStarted drives handle_task_started ->
    // insert_session_task_mapping(task_id -> tool_use_id).
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "long-running background agent".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
    // Backgrounding sentinel: the immediate tool_result flips the root's
    // card terminal while the agent keeps running (the round-1 false-
    // terminal). After this, only the session-map INTERSECT registry can
    // keep it alive across the turn reset below.
    send_msg(
        &mut app,
        user_message(vec![tool_result_block(
            "toolu_root",
            serde_json::json!("Agent launched in background. Task ID: task-root"),
        )]),
    );
    assert_eq!(
        tool_call_block(&app, "toolu_root").status,
        model::ToolCallStatus::Completed,
        "sentinel must flip the root card terminal, else the test doesn't exercise the backstop",
    );
    // The CLI registry lists it as a live backgrounded agent.
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-root",
                "task_type": "local_agent",
                "description": "long-running background agent",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    // Turn finalisation wipes the turn-scoped liveness; the session map
    // must carry the root across.
    let _: () = app.with_turn_state_mut(|ts| {
        ts.task_tool_use_ids.clear();
    });

    let view = app.subagents_view();
    assert_eq!(
        view.len(),
        1,
        "backgrounded agent survives turn reset via the real producer path; got {view:?}",
    );
    assert_eq!(view[0].tool_use_id, "toolu_root");
    assert_eq!(view[0].status, model::ToolCallStatus::InProgress);
    assert_eq!(view[0].tail.len(), 1, "its live tool tail is preserved; got {:?}", view[0].tail);
}

/// Producer-path sibling of
/// `backgrounded_agent_survives_turn_reset_over_real_wire_path` for a
/// backgrounded `local_bash`. Drives the real wire (`handle_task_started`
/// -> session map, backgrounding sentinel -> Completed card,
/// `handle_background_tasks_changed` -> registry) then resets the turn, and
/// asserts the session-scoped state the PROCESSES feed reads survives: the
/// registry lists it as `local_bash` and the session task map still
/// resolves its tool_use_id. The rendered enrichment
/// (`collect_active_processes` + OS-snapshot match) is exercised in-crate -
/// an OS snapshot can't be injected from an integration crate - so this
/// guards the producer an arg-swap in `handle_task_started` would silently
/// break.
#[tokio::test]
async fn backgrounded_bash_survives_turn_reset_over_real_wire_path() {
    let mut app = test_app();

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_bash",
            "Bash",
            serde_json::json!({
                "command": "sleep 60 && echo done",
                "description": "Wait then print",
                "run_in_background": true,
            }),
        )]),
    );
    // Real producer: TaskStarted -> insert_session_task_mapping(task_id -> tool_use_id).
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-bash".to_owned(),
            description: "Wait then print".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_bash".to_owned()),
            task_type: Some("local_bash".to_owned()),
        },
    );
    // Backgrounding sentinel flips the card terminal while the process runs -
    // the state where only the session-scoped registry can keep it enriched.
    send_msg(
        &mut app,
        user_message(vec![tool_result_block(
            "toolu_bash",
            serde_json::json!("Command running in background. Task ID: task-bash"),
        )]),
    );
    assert_eq!(
        tool_call_block(&app, "toolu_bash").status,
        model::ToolCallStatus::Completed,
        "sentinel must flip the card terminal, else the test doesn't exercise the backstop",
    );
    // CLI registry lists it as a live backgrounded local_bash.
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-bash",
                "task_type": "local_bash",
                "description": "Wait then print",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    // Turn finalisation wipes the turn-scoped liveness.
    let _: () = app.with_turn_state_mut(|ts| {
        ts.task_tool_use_ids.clear();
    });

    let session = app.active_session().expect("active session");
    assert_eq!(
        session.session_task_tool_use_ids.get("task-bash").map(String::as_str),
        Some("toolu_bash"),
        "session task map resolves the backgrounded bash across turn reset",
    );
    assert!(
        session
            .background_tasks
            .iter()
            .any(|task| task.task_id == "task-bash" && task.task_type == "local_bash"),
        "registry still lists it as local_bash; got {:?}",
        session.background_tasks,
    );
}

/// Sibling of `backgrounded_agent_survives_turn_reset_over_real_wire_path`
/// that drives the REAL turn-complete (`Message::Result`) rather than
/// hand-clearing the turn state: the same `apply_result_finalize` the CLI
/// triggers resets `SessionTurnState` AND runs the finalize sweep. With no
/// terminal `task_updated` yet, the backgrounded agent must stay active in
/// SUBAGENTS across that real boundary; a terminal `task_updated` then
/// clears the section.
#[tokio::test]
async fn backgrounded_agent_survives_real_turn_complete() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "long-running background agent",
                "prompt": "long-running background agent",
            }),
        )]),
    );
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block(
                "toolu_child",
                "Read",
                serde_json::json!({"file": "conv-row.tsx"}),
            )],
            "toolu_root",
        ),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "long-running background agent".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
    // Backgrounding sentinel flips the root card terminal while it runs.
    send_msg(
        &mut app,
        user_message(vec![tool_result_block(
            "toolu_root",
            serde_json::json!("Agent launched in background. Task ID: task-root"),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-root",
                "task_type": "local_agent",
                "description": "long-running background agent",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );

    // Drive the REAL turn-complete: SessionTurnState resets and the finalize
    // sweep runs, with no terminal task_updated seen yet.
    send_msg(&mut app, result_success_message());

    let view = app.subagents_view();
    assert_eq!(
        view.len(),
        1,
        "backgrounded agent stays in SUBAGENTS across the real turn-complete; got {view:?}",
    );
    assert_eq!(view[0].status, model::ToolCallStatus::InProgress);
    assert_eq!(view[0].tail.len(), 1, "its live tool tail survives; got {:?}", view[0].tail);

    // The terminal task_updated is a backgrounding sentinel, not the true
    // completion - the roster-alive agent stays visible across it.
    send_msg(
        &mut app,
        forge_primitives::Message::TaskUpdated {
            task_id: "task-root".to_owned(),
            patch: forge_primitives::messages::TaskUpdatePatch {
                status: Some("completed".to_owned()),
                end_time: None,
            },
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    assert_eq!(
        app.subagents_view().len(),
        1,
        "the sentinel terminal task_updated keeps the roster-alive agent visible; got {:?}",
        app.subagents_view(),
    );

    // True completion: the CLI drops the task from the roster, which clears
    // the section.
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: Vec::new(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    assert!(
        app.subagents_view().is_empty(),
        "the roster drop clears the section; got {:?}",
        app.subagents_view(),
    );
}

/// The turn boundary must not force-complete a backgrounded tool call whose
/// card is still open. Driving the REAL `Message::Result` runs the finalize
/// sweep, but a `run_in_background` Bash still in the session roster is
/// exempt, so its chat card stays `InProgress` instead of getting an
/// unearned checkmark. Without the exemption the sweep flips it `Completed`.
#[tokio::test]
async fn turn_end_does_not_force_complete_a_backgrounded_bash_card() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_bash",
            "Bash",
            serde_json::json!({
                "command": "sleep 60 && echo done",
                "description": "Wait then print",
                "run_in_background": true,
            }),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-bash".to_owned(),
            description: "Wait then print".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_bash".to_owned()),
            task_type: Some("local_bash".to_owned()),
        },
    );
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-bash",
                "task_type": "local_bash",
                "description": "Wait then print",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    // No backgrounding sentinel: the card is still InProgress at turn end -
    // the window the finalize sweep would otherwise force to Completed.
    assert_eq!(
        tool_call_block(&app, "toolu_bash").status,
        model::ToolCallStatus::InProgress,
        "precondition: the card is open before the turn ends",
    );

    // Drive the REAL turn-complete: finalize sweep + SessionTurnState reset.
    send_msg(&mut app, result_success_message());

    assert_eq!(
        tool_call_block(&app, "toolu_bash").status,
        model::ToolCallStatus::InProgress,
        "a roster-backed backgrounded card is exempt from the turn-end sweep",
    );
    // The session-scoped roster the PROCESSES enrichment reads survived.
    let session = app.active_session().expect("active session");
    assert!(
        session.background_tasks.iter().any(|task| task.task_id == "task-bash"),
        "the CLI registry still lists it as live; got {:?}",
        session.background_tasks,
    );
    assert_eq!(
        session.session_task_tool_use_ids.get("task-bash").map(String::as_str),
        Some("toolu_bash"),
        "the session task map still resolves it across the boundary",
    );
}

/// An InProgress backgrounded Bash card (no sentinel), for teardown tests.
fn backgrounded_bash_card(id: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: id.to_owned(),
        title: format!("tool {id}"),
        sdk_tool_name: "Bash".to_owned(),
        raw_input: None,
        raw_input_bytes: 0,
        output_metadata: None,
        task_metadata: None,
        status: model::ToolCallStatus::InProgress,
        content: Vec::new(),
        hidden: false,
        terminal_id: None,
        terminal_command: None,
        terminal_output: None,
        terminal_output_len: 0,
        terminal_bytes_seen: 0,
        terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
        monitor_output_tail: Vec::default(),
        render_epoch: 0,
        layout_epoch: 0,
        last_measured_width: 0,
        last_measured_height: 0,
        last_measured_layout_epoch: 0,
        last_measured_layout_generation: 0,
        cache: BlockCache::default(),
        collapsed_override: None,
        last_measured_y_in_msg: 0,
        answered_questions: Vec::new(),
    }
}

/// Seed the ACTIVE session over the wire with a mapped, still-open
/// backgrounded bash (tool_use + task_started + registry).
fn seed_active_backgrounded_bash(app: &mut App) {
    send_msg(
        app,
        assistant_message(vec![tool_use_block(
            "toolu_bash",
            "Bash",
            serde_json::json!({
                "command": "sleep 60 && echo done",
                "description": "Wait then print",
                "run_in_background": true,
            }),
        )]),
    );
    send_msg(
        app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-bash".to_owned(),
            description: "Wait then print".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_bash".to_owned()),
            task_type: Some("local_bash".to_owned()),
        },
    );
    send_msg(
        app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-bash",
                "task_type": "local_bash",
                "description": "Wait then print",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
}

/// A non-active bucket carrying a mapped, still-open backgrounded bash card.
fn bg_bucket_with_backgrounded_bash(key: &SessionKey) -> UiSession {
    let mut session = UiSession::new(key.clone());
    session.messages.push(ChatMessage::new(
        MessageRole::Assistant,
        vec![MessageBlock::ToolCall(Box::new(backgrounded_bash_card("toolu_bash")))],
        None,
    ));
    session.session_task_tool_use_ids.insert("task-bash".to_owned(), "toolu_bash".to_owned());
    session.background_tasks.push(BackgroundTask {
        task_id: "task-bash".to_owned(),
        task_type: "local_bash".to_owned(),
        description: "Wait then print".to_owned(),
    });
    session
}

fn bucket_card_status(session: &UiSession, id: &str) -> Option<model::ToolCallStatus> {
    session.messages.iter().flat_map(|m| &m.blocks).find_map(|b| match b {
        MessageBlock::ToolCall(tc) if tc.id == id => Some(tc.status),
        _ => None,
    })
}

// Teardown is a hard terminal: a session that dies must NOT keep a
// backgrounded card exempt from the sweep - the task can't complete on a dead
// session, so its card belongs at Failed, not stranded InProgress. The roster
// is cleared before the sweep at all four teardown sites (connection-failed +
// auth-required, active + background); each gets its own guard so a future
// revert of any one reorder fails loudly.

#[tokio::test]
async fn connection_failed_active_teardown_fails_a_backgrounded_card() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;
    seed_active_backgrounded_bash(&mut app);
    assert_eq!(
        tool_call_block(&app, "toolu_bash").status,
        model::ToolCallStatus::InProgress,
        "precondition: the backgrounded card is open before the session dies",
    );

    let key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::ConnectionFailed {
            key,
            message: "connection lost".to_owned(),
            fatal: false,
        },
    );

    assert_eq!(
        tool_call_block(&app, "toolu_bash").status,
        model::ToolCallStatus::Failed,
        "a dead active session's backgrounded card is failed, not stranded",
    );
}

#[tokio::test]
async fn auth_required_active_teardown_fails_a_backgrounded_card() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;
    seed_active_backgrounded_bash(&mut app);

    let key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::AuthRequired {
            key,
            method_name: "claude.ai".to_owned(),
            method_description: String::new(),
        },
    );

    assert_eq!(
        tool_call_block(&app, "toolu_bash").status,
        model::ToolCallStatus::Failed,
        "an auth-blocked active session's backgrounded card is failed, not stranded",
    );
}

#[tokio::test]
async fn connection_failed_background_teardown_fails_a_backgrounded_card() {
    let mut app = test_app();
    // Establish a different active session so the target is genuinely non-active.
    send_msg(&mut app, assistant_message(vec![text_block("active")]));
    let bg_key = SessionKey::from_str_for_test("bg-session");
    app.sessions.insert(bg_key.clone(), bg_bucket_with_backgrounded_bash(&bg_key));

    send_client_event(
        &mut app,
        SessionUpdate::ConnectionFailed {
            key: bg_key.clone(),
            message: "connection lost".to_owned(),
            fatal: false,
        },
    );

    let bg = app.sessions.get(&bg_key).expect("bg bucket");
    assert_eq!(
        bucket_card_status(bg, "toolu_bash"),
        Some(model::ToolCallStatus::Failed),
        "a dead background session's backgrounded card is failed, not stranded",
    );
}

#[tokio::test]
async fn auth_required_background_teardown_fails_a_backgrounded_card() {
    let mut app = test_app();
    send_msg(&mut app, assistant_message(vec![text_block("active")]));
    let bg_key = SessionKey::from_str_for_test("bg-session");
    app.sessions.insert(bg_key.clone(), bg_bucket_with_backgrounded_bash(&bg_key));

    send_client_event(
        &mut app,
        SessionUpdate::AuthRequired {
            key: bg_key.clone(),
            method_name: "claude.ai".to_owned(),
            method_description: String::new(),
        },
    );

    let bg = app.sessions.get(&bg_key).expect("bg bucket");
    assert_eq!(
        bucket_card_status(bg, "toolu_bash"),
        Some(model::ToolCallStatus::Failed),
        "an auth-blocked background session's backgrounded card is failed, not stranded",
    );
}

/// No-leak lock for the backgrounded-bash card on genuine completion. With
/// the realistic sentinel flow the backgrounding tool_result flips the card
/// `Completed` before `result`, so it is already terminal at turn-end and
/// the post-turn `task_updated` mapping loss is harmless. After the real
/// Result the card + roster survive; a terminal `task_updated` plus empty
/// `background_tasks` then leave the card terminal and drain the roster +
/// session map (the inputs the PROCESSES synthetic feed reads), so no
/// backgrounded row can linger.
#[tokio::test]
async fn backgrounded_bash_card_and_roster_clear_on_genuine_completion() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_bash",
            "Bash",
            serde_json::json!({
                "command": "sleep 60 && echo done",
                "description": "Wait then print",
                "run_in_background": true,
            }),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-bash".to_owned(),
            description: "Wait then print".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_bash".to_owned()),
            task_type: Some("local_bash".to_owned()),
        },
    );
    // Backgrounding sentinel flips the card terminal while the process runs.
    send_msg(
        &mut app,
        user_message(vec![tool_result_block(
            "toolu_bash",
            serde_json::json!("Command running in background. Task ID: task-bash"),
        )]),
    );
    assert_eq!(
        tool_call_block(&app, "toolu_bash").status,
        model::ToolCallStatus::Completed,
        "the sentinel flips the card terminal before the turn ends",
    );
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-bash",
                "task_type": "local_bash",
                "description": "Wait then print",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );

    // Real turn-complete: card + roster survive across it.
    send_msg(&mut app, result_success_message());
    assert_eq!(tool_call_block(&app, "toolu_bash").status, model::ToolCallStatus::Completed);
    assert!(
        app.active_session()
            .expect("session")
            .background_tasks
            .iter()
            .any(|t| t.task_id == "task-bash"),
        "roster still lists the task after the real Result",
    );

    // Genuine completion: terminal task_updated + empty background_tasks.
    send_msg(
        &mut app,
        forge_primitives::Message::TaskUpdated {
            task_id: "task-bash".to_owned(),
            patch: forge_primitives::messages::TaskUpdatePatch {
                status: Some("killed".to_owned()),
                end_time: None,
            },
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: Vec::new(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );

    // The card is terminal (settled by the pre-Result sentinel), and the
    // roster + session map have drained - nothing feeds a lingering row.
    assert_eq!(
        tool_call_block(&app, "toolu_bash").status,
        model::ToolCallStatus::Completed,
        "card stays terminal on completion; got {:?}",
        tool_call_block(&app, "toolu_bash").status,
    );
    let session = app.active_session().expect("session");
    assert!(
        session.background_tasks.is_empty(),
        "roster drained; got {:?}",
        session.background_tasks
    );
    assert!(
        !session.session_task_tool_use_ids.contains_key("task-bash"),
        "session map drained - the PROCESSES synthetic feed has no source",
    );
}

/// Regression (2.1.204 local-agent streaming): a subagent's assistant
/// message carries `parent_tool_use_id` on the envelope and now streams
/// its narration text into the parent's live wire. Its tool calls scope
/// to the SUBAGENTS inspector, but its narration text must NOT leak into
/// the main chat.
#[tokio::test]
async fn subagent_assistant_narration_does_not_leak_into_main_chat() {
    let mut app = test_app();

    // Main-agent narration must still render (guard against over-suppression).
    send_msg(&mut app, assistant_message(vec![text_block("main agent line")]));
    // Subagent narration (parent_tool_use_id set) must be suppressed.
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![text_block("subagent narration leaking")],
            "toolu-parent",
        ),
    );

    let chat_text: String = app
        .messages()
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            MessageBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(chat_text.contains("main agent line"), "main-agent narration must still render");
    assert!(
        !chat_text.contains("subagent narration leaking"),
        "subagent narration must not leak into the main chat; got {chat_text:?}",
    );
}
