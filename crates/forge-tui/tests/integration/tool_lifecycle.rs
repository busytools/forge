// =====
// TESTS: 29
// =====
//
// Tool call lifecycle integration tests.
// Validates the full create -> update -> complete flow for tool calls
// over the wire-message dispatch path.

use forge_tui::agent::model;
use forge_tui::app::session::UiSession;
use forge_tui::app::{
    App, AppStatus, BackgroundTask, BlockCache, ChatMessage, MessageBlock, MessageRole,
    ToolCallInfo, ToolCallScope,
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

/// The frames every backgrounded-subagent case starts from: dispatch an
/// `Agent`, let the CLI acknowledge the background launch, and put it in
/// the roster. Root is `toolu_root`, task is `task-root`.
fn seed_backgrounded_subagent(app: &mut App) {
    send_msg(
        app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({"subagent_type": "Explore", "description": "d", "prompt": "d"}),
        )]),
    );
    send_msg(
        app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "d".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
    send_msg(
        app,
        user_message(vec![tool_result_block(
            "toolu_root",
            serde_json::json!("Agent launched in background. Task ID: task-root"),
        )]),
    );
    send_msg(
        app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-root",
                "task_type": "local_agent",
                "description": "d",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
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

/// A roster frame arriving AFTER the turn's Result must not collapse the
/// subagent exemption. At the boundary the roster is momentarily empty,
/// so a snapshot-only liveness read drops the root + child scopes and
/// sweeps the child, and the late frame restores nothing (#790). The
/// sticky per-session marker set at `task_started` is the historical
/// half of the signal that survives that window.
#[tokio::test]
async fn a_late_roster_frame_does_not_collapse_the_subagent_exemption() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    // Dispatch + task_started; the roster frame has NOT arrived yet.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "bg scan",
                "prompt": "bg scan",
            }),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "bg scan".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_child", "Bash", serde_json::json!({"command": "x"}))],
            "toolu_root",
        ),
    );

    // The spawning turn completes while the roster is still empty.
    send_msg(&mut app, result_success_message());

    // The roster lands a frame late.
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-root",
                "task_type": "local_agent",
                "description": "bg scan",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );

    assert_eq!(
        app.tool_call_scope("toolu_root"),
        Some(ToolCallScope::SubagentRoot),
        "the root's scope survives a roster that arrived late; got {:?}",
        app.tool_call_scope("toolu_root"),
    );
    assert_eq!(
        app.tool_call_scope("toolu_child"),
        Some(ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() }),
        "the child's scope survives too; got {:?}",
        app.tool_call_scope("toolu_child"),
    );
    assert_eq!(
        tool_call_block(&app, "toolu_child").status,
        model::ToolCallStatus::InProgress,
        "the child was not swept by the boundary; got {:?}",
        tool_call_block(&app, "toolu_child").status,
    );
    assert_eq!(
        app.subagents_view().len(),
        1,
        "SUBAGENTS still shows the agent; got {:?}",
        app.subagents_view(),
    );
}

/// The sticky marker bridges the task_started -> first-roster-frame
/// window, but it is liveness, not immortality: only a terminal
/// `task_updated` (with the roster drain that accompanies it) may end
/// it. Nothing else - not a turn boundary, not a card transition -
/// clears it.
#[tokio::test]
async fn the_sticky_backgrounded_root_lasts_until_a_terminal_event() {
    fn alive_of(app: &App) -> std::collections::HashSet<String> {
        app.active_session()
            .expect("active session")
            .backgrounded_alive_tool_use_ids()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "bg scan",
                "prompt": "bg scan",
            }),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "bg scan".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );

    // The turn completes before any roster frame: the sticky marker
    // carries liveness through the boundary the empty roster would
    // otherwise collapse.
    send_msg(&mut app, result_success_message());
    assert!(
        alive_of(&app).contains("toolu_root"),
        "the sticky root carries liveness across the boundary; got {:?}",
        alive_of(&app),
    );

    // The late roster frame lands; liveness continues.
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-root",
                "task_type": "local_agent",
                "description": "bg scan",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    assert!(
        alive_of(&app).contains("toolu_root"),
        "rostered liveness continues; got {:?}",
        alive_of(&app),
    );

    // True completion: terminal task_updated, then the drain frame.
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
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: Vec::new(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    assert!(
        alive_of(&app).is_empty(),
        "nothing may keep the root alive past its terminal event; got {:?}",
        alive_of(&app),
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
        terminal_output: None,
        monitor_output_tail: Vec::default(),
        monitor_status: None,
        render_epoch: 0,
        layout_epoch: 0,
        last_measured_width: 0,
        last_measured_height: 0,
        last_measured_layout_epoch: 0,
        last_measured_layout_generation: 0,
        last_measured_tools_collapsed: false,
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

/// A backgrounded subagent's children keep arriving after the turn that
/// dispatched it Resulted. They must not claim the finished turn: no
/// indicator on it, and the session must not read busy for the
/// subagent's whole life. Two reviewers measured the coverage this
/// comment used to claim and both found it absent: widening
/// `subagent_scoped` to include the root survives here, and so does
/// dropping either bind guard. What it does catch is the status write.
#[tokio::test]
async fn backgrounded_subagent_traffic_does_not_reopen_the_finished_turn() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "general-purpose",
                "description": "bg scan",
                "prompt": "bg scan",
            }),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "bg scan".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
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
                "description": "bg scan",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    send_msg(&mut app, result_success_message());

    let ended_turn = app.messages().len() - 1;
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_child", "Bash", serde_json::json!({"command": "rg x"}))],
            "toolu_root",
        ),
    );

    assert_eq!(
        app.active_turn_assistant_idx(),
        None,
        "a subagent child must not bind the main agent's turn container",
    );
    assert!(
        matches!(app.status, AppStatus::Ready),
        "a subagent's own call is not this session working; got {:?}",
        app.status,
    );
    assert_eq!(
        app.messages().len() - 1,
        ended_turn,
        "the child belongs beside the turn that dispatched it, not in a new one",
    );
    assert_eq!(app.subagents_view().len(), 1, "the Inspector still owns the running subagent");

    // Something new after the turn is its own turn, and says so.
    send_msg(&mut app, assistant_message(vec![text_block("fresh")]));
    assert_eq!(
        app.active_turn_assistant_idx(),
        Some(ended_turn + 1),
        "new streaming text opens its own turn rather than joining the dead one",
    );
}

/// A turn that opens with a tool_use, while the previous turn's message
/// is still the tail, must not land on it - that glued two turns into
/// one message and left the second turn's duration rendering nowhere.
/// Catches dropping the `tail_turn_ended` conjunct.
#[tokio::test]
async fn a_turn_opening_with_a_tool_use_does_not_land_on_the_previous_turn() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    send_msg(&mut app, assistant_message(vec![text_block("first turn")]));
    let mut first = result_success_message();
    if let forge_primitives::Message::Result { ref mut duration_ms, .. } = first {
        *duration_ms = 3_174;
    }
    send_msg(&mut app, first);
    assert_eq!(app.messages().len(), 1, "one turn so far");

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc_second",
            "Bash",
            serde_json::json!({"command": "ls"}),
        )]),
    );
    send_msg(&mut app, assistant_message(vec![text_block("second turn")]));

    assert_eq!(app.messages().len(), 2, "the second turn owns its own message");
    assert_eq!(
        app.messages()[1].turn_info.duration_ms,
        None,
        "the new turn must not inherit the finished turn's duration",
    );
    assert_eq!(
        app.messages()[0].turn_info.duration_ms,
        Some(3_174),
        "and the finished turn keeps its own",
    );
}

/// Ved's call: the chat goes quiet at turn end but the tab title keeps
/// showing activity while a backgrounded subagent runs - the chat is
/// where you are looking, the title is how you know to look. It must
/// hold on real background activity rather than on `app.status`, which
/// reads Ready here. The wiring is compiler-enforced: `update_tab_title`
/// takes a bool, so it cannot be keyed on a status.
#[tokio::test]
async fn tab_title_shows_activity_after_turn_end_while_background_work_runs() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    seed_backgrounded_subagent(&mut app);
    send_msg(&mut app, result_success_message());

    // The subagent keeps emitting its own envelopes after the turn ends.
    // These must not put the bucket back in flight - no Result follows
    // them, so a bucket flipped to Running here never comes back.
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_child", "Bash", serde_json::json!({"command": "x"}))],
            "toolu_root",
        ),
    );

    assert!(
        matches!(app.status, AppStatus::Ready),
        "the turn ended, so the session is not busy; got {:?}",
        app.status,
    );
    assert!(
        app.shows_activity(),
        "the title must still show activity, on the live background work rather than a stale status",
    );

    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: Vec::new(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    assert!(
        !app.shows_activity(),
        "with the background work drained and the turn over, the title goes idle",
    );
}

/// The background-session sweep takes the same exemption as the two
/// active-path ones, so a live backgrounded subagent's children survive a
/// turn ending in a bucket the user is not looking at. Catches that sweep
/// narrowing back to the roster while the other two keep the wider set -
/// which is the drift a single shared helper exists to prevent.
#[tokio::test]
async fn the_background_sweep_spares_a_live_backgrounded_subagents_children() {
    let mut app = test_app();
    // Somebody else is active, so the target bucket is genuinely background.
    send_msg(&mut app, assistant_message(vec![text_block("active")]));

    let bg_key = SessionKey::from_str_for_test("bg-subagent");
    let mut bg = UiSession::new(bg_key.clone());
    let mut root = backgrounded_bash_card("toolu_root");
    root.sdk_tool_name = "Agent".to_owned();
    let child = backgrounded_bash_card("toolu_child");
    bg.messages.push(ChatMessage::new(
        MessageRole::Assistant,
        vec![MessageBlock::ToolCall(Box::new(root)), MessageBlock::ToolCall(Box::new(child))],
    ));
    bg.tool_call_scopes.insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
    bg.tool_call_scopes.insert(
        "toolu_child".to_owned(),
        ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
    );
    bg.session_task_tool_use_ids.insert("task-root".to_owned(), "toolu_root".to_owned());
    bg.background_tasks.push(BackgroundTask {
        task_id: "task-root".to_owned(),
        task_type: "local_agent".to_owned(),
        description: "bg scan".to_owned(),
    });
    app.sessions.insert(bg_key.clone(), bg);

    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: bg_key.clone(), terminal_reason: None },
    );

    let bg = app.sessions.get(&bg_key).expect("bg bucket");
    assert_eq!(
        bucket_card_status(bg, "toolu_root"),
        Some(model::ToolCallStatus::InProgress),
        "the live root is exempt, as it already was",
    );
    assert_eq!(
        bucket_card_status(bg, "toolu_child"),
        Some(model::ToolCallStatus::InProgress),
        "and so is its still-running child",
    );
}

/// A `Task` issued by a subagent registers as that subagent's child, not a
/// root, so its own children reach the roster only through the chain above
/// them. Walking one hop left them looking unowned and swept while running
/// - `Completed` at a Result, which asserts success for work still going.
/// Catches the exemption going back to a single hop.
#[tokio::test]
async fn a_grandchild_of_a_live_backgrounded_subagent_is_spared() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;
    seed_backgrounded_subagent(&mut app);
    // A `Task` issued BY the subagent registers as that subagent's child,
    // so its own child reaches the roster only through the chain above it.
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_nested", "Task", serde_json::json!({"description": "n"}))],
            "toolu_root",
        ),
    );
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_gchild", "Bash", serde_json::json!({"command": "sleep"}))],
            "toolu_nested",
        ),
    );

    send_msg(&mut app, result_success_message());
    assert!(
        matches!(tool_call_block(&app, "toolu_gchild").status, model::ToolCallStatus::InProgress),
        "a grandchild must not be reported Completed while it runs; got {:?}",
        tool_call_block(&app, "toolu_gchild").status,
    );

    app.finalize_in_progress_tool_calls(model::ToolCallStatus::Failed);
    assert!(
        matches!(tool_call_block(&app, "toolu_gchild").status, model::ToolCallStatus::InProgress),
        "nor Failed by the submit-path sweep; got {:?}",
        tool_call_block(&app, "toolu_gchild").status,
    );
}

/// Once a backgrounded root drains from the roster, nothing sweeps its
/// still-open children until the next turn boundary - an idle session
/// shows a running card and a PROCESSES row for work that is done. The
/// roster diff itself is the settle signal: the departed root's open
/// children go terminal at the drain, while the root keeps its card for
/// its own `task_updated`, which lands a frame later.
#[tokio::test]
async fn a_roster_drain_settles_the_departed_roots_open_children() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "bg scan",
                "prompt": "bg scan",
            }),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "bg scan".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-root",
                "task_type": "local_agent",
                "description": "bg scan",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_child", "Bash", serde_json::json!({"command": "sleep"}))],
            "toolu_root",
        ),
    );
    assert_eq!(
        tool_call_block(&app, "toolu_child").status,
        model::ToolCallStatus::InProgress,
        "precondition: the child card is open",
    );

    // The spawning turn completes; the rostered root spares both cards.
    send_msg(&mut app, result_success_message());
    assert_eq!(
        tool_call_block(&app, "toolu_child").status,
        model::ToolCallStatus::InProgress,
        "precondition: the rostered root's child survives the turn boundary",
    );

    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: Vec::new(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );
    assert_eq!(
        tool_call_block(&app, "toolu_child").status,
        model::ToolCallStatus::Completed,
        "the departed root's open child settles at the roster drain; got {:?}",
        tool_call_block(&app, "toolu_child").status,
    );
    assert_eq!(
        tool_call_block(&app, "toolu_root").status,
        model::ToolCallStatus::InProgress,
        "the root is not swept with its children; got {:?}",
        tool_call_block(&app, "toolu_root").status,
    );
}

/// A turn error kills the TURN, not backgrounded work: a root marked
/// sticky at `task_started` (and its open child) is exempt from the
/// Failed sweep, matching the cancel path and the background-session
/// branch. The marker is session-lifetime state - wiping it here would
/// also unlive roots from earlier turns in the roster-lag window.
/// Its own terminal events still settle the cards later.
#[tokio::test]
async fn a_turn_error_spares_a_sticky_roots_cards() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    // Dispatch + task_started; no roster frame yet, so the sticky
    // marker is the only liveness signal in play.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "bg scan",
                "prompt": "bg scan",
            }),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "bg scan".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_child", "Bash", serde_json::json!({"command": "x"}))],
            "toolu_root",
        ),
    );
    assert_eq!(
        tool_call_block(&app, "toolu_root").status,
        model::ToolCallStatus::InProgress,
        "precondition: the root card is open",
    );

    let key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnError {
            key,
            message: "boom".to_owned(),
            class: None,
            terminal_reason: None,
        },
    );

    assert_eq!(
        tool_call_block(&app, "toolu_root").status,
        model::ToolCallStatus::InProgress,
        "the sticky root is exempt from the errored turn's sweep; got {:?}",
        tool_call_block(&app, "toolu_root").status,
    );
    assert_eq!(
        tool_call_block(&app, "toolu_child").status,
        model::ToolCallStatus::InProgress,
        "and so is its child; got {:?}",
        tool_call_block(&app, "toolu_child").status,
    );

    // The marker still clears on its own terminal events: the errored
    // turn reset the turn-scoped lookup, so the session map is what
    // resolves the terminal patch to the marker.
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
        app.active_session().expect("active session").backgrounded_alive_tool_use_ids().is_empty(),
        "the terminal event ends the sticky liveness; got {:?}",
        app.active_session().expect("active session").backgrounded_alive_tool_use_ids(),
    );
}

/// A subagent's own backgrounded work is self-rostered: a nested Task
/// carries its own roster row, its own `task_started` mapping and its
/// own terminal events, so draining the PARENT's row must not settle it
/// - nor anything still hanging off it. Only chains that reach the
/// departed root through no live work settle.
#[tokio::test]
async fn the_roster_drain_spares_a_descendant_that_is_live_on_its_own_roster_row() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_root",
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "description": "bg scan",
                "prompt": "bg scan",
            }),
        )]),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-root".to_owned(),
            description: "bg scan".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_root".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
    // A plain child of the root: nothing but the root keeps it alive.
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_plain", "Bash", serde_json::json!({"command": "x"}))],
            "toolu_root",
        ),
    );
    // A nested Task issued BY the subagent - itself a backgrounded
    // dispatch - plus a grandchild under it.
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_nested", "Task", serde_json::json!({"description": "n"}))],
            "toolu_root",
        ),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::TaskStarted {
            task_id: "task-nested".to_owned(),
            description: "n".to_owned(),
            uuid: String::new(),
            session_id: "test-session".to_owned(),
            tool_use_id: Some("toolu_nested".to_owned()),
            task_type: Some("local_agent".to_owned()),
        },
    );
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_gchild", "Bash", serde_json::json!({"command": "y"}))],
            "toolu_nested",
        ),
    );
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![
                serde_json::json!({
                    "task_id": "task-root",
                    "task_type": "local_agent",
                    "description": "bg scan",
                }),
                serde_json::json!({
                    "task_id": "task-nested",
                    "task_type": "local_agent",
                    "description": "n",
                }),
            ],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );

    // The root's row drains; the nested task's row stays.
    send_msg(
        &mut app,
        forge_primitives::Message::BackgroundTasksChanged {
            tasks: vec![serde_json::json!({
                "task_id": "task-nested",
                "task_type": "local_agent",
                "description": "n",
            })],
            uuid: String::new(),
            session_id: "test-session".to_owned(),
        },
    );

    assert_eq!(
        tool_call_block(&app, "toolu_plain").status,
        model::ToolCallStatus::Completed,
        "the plain child settles at the drain; got {:?}",
        tool_call_block(&app, "toolu_plain").status,
    );
    assert_eq!(
        tool_call_block(&app, "toolu_nested").status,
        model::ToolCallStatus::InProgress,
        "self-rostered work survives its parent's drain; got {:?}",
        tool_call_block(&app, "toolu_nested").status,
    );
    assert_eq!(
        tool_call_block(&app, "toolu_gchild").status,
        model::ToolCallStatus::InProgress,
        "and so does everything hanging off the nested task; got {:?}",
        tool_call_block(&app, "toolu_gchild").status,
    );
}

/// A `Task`/`Agent` dispatch is the MAIN agent's own call, so a turn that
/// opens with one owns its turn like any other. Classifying the root as
/// subagent-scoped bars it from binding and reproduces the finished-turn
/// symptom one level up, on the dispatching turn itself.
#[tokio::test]
async fn a_turn_opening_with_a_subagent_dispatch_still_owns_its_turn() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;
    send_msg(&mut app, assistant_message(vec![text_block("first turn")]));
    let mut first = result_success_message();
    if let forge_primitives::Message::Result { ref mut duration_ms, .. } = first {
        *duration_ms = 1_000;
    }
    send_msg(&mut app, first);
    assert_eq!(app.active_turn_assistant_idx(), None, "the first turn released the anchor");

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "toolu_dispatch",
            "Agent",
            serde_json::json!({"subagent_type": "Explore", "description": "d", "prompt": "d"}),
        )]),
    );

    let dispatch_msg = app.lookup_tool_call("toolu_dispatch").expect("indexed").0;
    assert_eq!(
        app.active_turn_assistant_idx(),
        Some(dispatch_msg),
        "the dispatching turn owns the message it opened",
    );
    assert!(
        matches!(app.status, AppStatus::Running),
        "and the dispatch is this session working; got {:?}",
        app.status,
    );
}

/// The orphan path: a child whose root is no longer in the index, which
/// retention produces routinely - it drops from the front where a root
/// sits, and a backgrounded root's card goes terminal as soon as the CLI
/// acknowledges the launch, so it is not retention-protected.
///
/// Reusing the last assistant message rather than the tail is what bounds
/// this: a notice makes the tail non-assistant every episode, so a tail
/// rule pushes again each time. The count must be flat after the first
/// arrival, and this is the case the root-present loop cannot reach.
#[tokio::test]
async fn orphaned_subagent_children_do_not_accumulate_assistant_messages() {
    let assistants = |app: &App| {
        app.messages().iter().filter(|m| matches!(m.role, MessageRole::Assistant)).count()
    };

    let mut app = test_app();
    app.status = AppStatus::Thinking;
    send_msg(&mut app, assistant_message(vec![text_block("a turn")]));
    send_msg(&mut app, result_success_message());

    let mut settled: Option<usize> = None;
    for i in 0..10 {
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::System(None),
            vec![MessageBlock::Text(forge_tui::app::TextBlock::from_complete("notice"))],
        ));
        // The parent id names a tool call that is not in the index.
        send_msg(
            &mut app,
            assistant_message_with_parent(
                vec![tool_use_block(
                    &format!("toolu_orphan_{i}"),
                    "Bash",
                    serde_json::json!({"command": "x"}),
                )],
                "toolu_evicted_root",
            ),
        );
        let now = assistants(&app);
        match settled {
            None => settled = Some(now),
            Some(expected) => assert_eq!(
                now, expected,
                "episode {i} added an assistant message; orphans must reuse the last one",
            ),
        }
        assert_eq!(
            app.active_turn_assistant_idx(),
            None,
            "episode {i}: an orphan must not claim a turn container",
        );
    }

    // Placement is deliberately unconstrained beyond boundedness: an
    // orphan still lands in some turn's message, which is the attribution
    // error #790 has to close.
    assert_eq!(assistants(&app), settled.expect("ten episodes ran"), "flat throughout");
}

/// The placement rule is "the root's message", not "the last assistant".
/// Every other test has those be the same message, so they cannot tell
/// the rule from the fallback. This drives a later main-agent turn to
/// move the last assistant away from the root, then delivers a child.
#[tokio::test]
async fn a_subagent_child_follows_its_root_not_the_last_assistant() {
    let mut app = test_app();
    app.status = AppStatus::Thinking;
    seed_backgrounded_subagent(&mut app);
    send_msg(&mut app, result_success_message());
    let root_msg = app.lookup_tool_call("toolu_root").expect("root indexed").0;

    // A later main-agent turn: the last assistant is now somewhere else.
    send_msg(&mut app, assistant_message(vec![text_block("a later turn")]));
    send_msg(&mut app, result_success_message());
    let last_assistant = app
        .messages()
        .iter()
        .rposition(|m| matches!(m.role, MessageRole::Assistant))
        .expect("an assistant exists");
    assert_ne!(last_assistant, root_msg, "the fixture must separate them or it proves nothing");

    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_child", "Bash", serde_json::json!({"command": "x"}))],
            "toolu_root",
        ),
    );

    assert_eq!(
        app.lookup_tool_call("toolu_child").map(|(m, _)| m),
        Some(root_msg),
        "the child follows its root, not whichever assistant happens to be last",
    );
}

/// A resumed agent's Task card replays WITHOUT a scope - resume
/// registers none and the post-replay sweep clears the rest - so a
/// dispatch-keyed view can never see it again, even while the resumed
/// agent's live child frames keep naming the card. The view must derive
/// a root from such a parent too (#808), and drain once nothing under
/// it runs.
///
/// The fixture encodes the premise the PR flags as unverified: a child
/// frame's `parent_tool_use_id` is the ORIGINAL dispatch's tool-use id
/// (read off subagents/*.meta.json). A live capture showing children
/// naming a different id leaves this derivation parentless and the
/// resumed agent invisible again - the capture task decides.
///
/// Composition note: in this resume state nothing restores liveness.
/// The replay walk synthesizes only User / Assistant envelopes, so no
/// `task_started`, roster row, or session-map entry is re-delivered,
/// and the first turn boundary sweeps the live child and drains the
/// section. Whether the resumed CLI announces its agents is what the
/// live capture pins; a `task_started` would re-arm the sticky marker
/// and a roster row re-arms it through the session map.
#[tokio::test]
async fn a_resumed_unscoped_root_surfaces_through_its_children_frames() {
    let mut app = test_app();

    // The replayed Task card: indexed, terminal from its replayed
    // result, no scope registered.
    let mut root = backgrounded_bash_card("toolu_root");
    root.sdk_tool_name = "Task".to_owned();
    root.status = model::ToolCallStatus::Completed;
    root.hidden = true;
    app.active_messages_mut().push(ChatMessage::new(
        MessageRole::Assistant,
        vec![MessageBlock::ToolCall(Box::new(root))],
    ));
    let root_msg = app.messages().len() - 1;
    app.index_tool_call("toolu_root".to_owned(), root_msg, 0);

    assert!(
        app.subagents_view().is_empty(),
        "precondition: an unscoped card alone shows nothing; got {:?}",
        app.subagents_view(),
    );

    // Live child traffic from the resumed agent names the card.
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block("toolu_child", "Bash", serde_json::json!({"command": "sleep"}))],
            "toolu_root",
        ),
    );

    let view = app.subagents_view();
    assert_eq!(view.len(), 1, "the resumed agent surfaces through its child frame; got {view:?}",);
    assert_eq!(view[0].tool_use_id, "toolu_root");
    assert_eq!(
        view[0].status,
        model::ToolCallStatus::InProgress,
        "it renders running on the open child; got {:?}",
        view[0].status,
    );

    // The child settles; nothing keeps the section open.
    send_msg(
        &mut app,
        user_message(vec![tool_result_block("toolu_child", serde_json::json!("ok"))]),
    );
    assert!(
        app.subagents_view().is_empty(),
        "the section drains once the child settles; got {:?}",
        app.subagents_view(),
    );
}
