//! Slash command types, parsing, and delegation.
//!
//! Submodules:
//! - `candidates`: candidate detection, filtering, and building
//! - `navigation`: autocomplete activation, movement, and confirm
//! - `executors`: slash command execution handlers

mod candidates;
mod executors;
mod navigation;

use super::{
    App, AppStatus, ChatMessage, MessageBlock, MessageRole, TextBlock, dialog::DialogState,
};
use crate::agent::model;

/// Visible rows in the slash dropdown. Capped at 20 so a long
/// command list (50+ between forge + claude groups) doesn't blow
/// the dropdown to two-thirds of the screen height; scrolling via
/// the dialog handles the overflow. The renderer additionally
/// clamps to whatever rows the terminal actually has above the
/// input cursor - the navigation math uses this same cap so the
/// rendered window and the dialog's scroll_offset agree.
pub const MAX_VISIBLE: usize = 20;
use super::MAX_CANDIDATES;

// Re-export public API
pub(crate) use candidates::is_sdk_default_model_option;
pub(crate) use executors::switch_model;
pub use executors::try_handle_submit;
pub use navigation::{
    activate, confirm_selection, deactivate, move_down, move_up, sync_with_cursor, update_query,
};

#[derive(Debug, Clone)]
pub struct SlashCandidate {
    pub insert_value: String,
    pub primary: String,
    pub secondary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashContext {
    CommandName,
    Argument { command: String, arg_index: usize, token_range: (usize, usize) },
}

#[derive(Debug, Clone)]
pub struct SlashState {
    /// Character position where `/` token starts.
    pub trigger_row: usize,
    pub trigger_col: usize,
    /// Current typed query for the active slash context.
    pub query: String,
    /// Command-name or argument context.
    pub context: SlashContext,
    /// Filtered list of supported candidates.
    pub candidates: Vec<SlashCandidate>,
    /// Shared autocomplete dialog navigation state.
    pub dialog: DialogState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlashDetection {
    trigger_row: usize,
    trigger_col: usize,
    query: String,
    context: SlashContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSlash<'a> {
    name: &'a str,
    args: Vec<&'a str>,
}

fn parse(text: &str) -> Option<ParsedSlash<'_>> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let name = parts.next()?;
    Some(ParsedSlash { name, args: parts.collect() })
}

fn normalize_slash_name(name: &str) -> String {
    if name.starts_with('/') { name.to_owned() } else { format!("/{name}") }
}

pub(crate) fn push_system_message(app: &mut App, text: impl Into<String>) {
    let text = text.into();
    app.push_message_tracked(ChatMessage::new(
        MessageRole::System(None),
        vec![MessageBlock::Text(TextBlock::from_complete(&text))],
    ));
    app.enforce_history_retention_tracked();
    app.active_viewport_mut().engage_auto_scroll();
}

/// Push an info-severity system message - the success / status
/// variant. `push_system_message` (severity `None`) renders as
/// red Error per `system_severity_from_role`; use this for non-
/// error feedback like `/mode` / `/model` / `/effort` no-arg
/// getters and successful "Set X to Y" confirmations.
pub(super) fn push_system_info(app: &mut App, text: impl Into<String>) {
    let text = text.into();
    app.push_message_tracked(ChatMessage::new(
        MessageRole::System(Some(super::SystemSeverity::Info)),
        vec![MessageBlock::Text(TextBlock::from_complete(&text))],
    ));
    app.enforce_history_retention_tracked();
    app.active_viewport_mut().engage_auto_scroll();
}

fn push_user_message(app: &mut App, text: impl Into<String>) {
    let text = text.into();
    app.push_message_tracked(ChatMessage::new(
        MessageRole::User,
        vec![MessageBlock::Text(TextBlock::from_complete(&text))],
    ));
    app.enforce_history_retention_tracked();
    app.active_viewport_mut().engage_auto_scroll();
}

fn require_connection(app: &mut App, not_connected_msg: &'static str) -> bool {
    if !app.has_active_agent() {
        push_system_message(app, not_connected_msg);
        return false;
    }
    true
}

pub(crate) fn require_active_session(
    app: &mut App,
    not_connected_msg: &'static str,
    no_session_msg: &'static str,
) -> Option<model::SessionId> {
    if !require_connection(app, not_connected_msg) {
        return None;
    }
    let Some(session_id) = app.session_id() else {
        push_system_message(app, no_session_msg);
        return None;
    };
    Some(session_id)
}

/// Block the input field while a slash command is in flight.
fn set_command_pending(app: &mut App, label: &str, ack: Option<super::PendingCommandAck>) {
    app.status = AppStatus::CommandPending;
    *app.pending_command_label_mut() = Some(label.to_owned());
    *app.pending_command_ack_mut() = ack;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use serde_json::json;

    // Re-import submodule items needed by tests
    use super::candidates::{
        argument_candidates, detect_slash_at_cursor, supported_command_candidates,
    };

    #[test]
    fn parse_non_slash_returns_none() {
        assert!(parse("hello world").is_none());
    }

    #[test]
    fn parse_slash_name_and_args() {
        let parsed = parse("/mode plan").expect("slash command");
        assert_eq!(parsed.name, "/mode");
        assert_eq!(parsed.args, vec!["plan"]);
    }

    #[test]
    fn unsupported_command_is_handled_locally() {
        let mut app = App::test_default();
        let consumed = try_handle_submit(&mut app, "/definitely-unknown");
        assert!(consumed);
        let Some(last) = app.messages().last() else {
            panic!("expected system message");
        };
        assert!(matches!(last.role, MessageRole::System(_)));
    }

    #[test]
    fn account_command_blocked_while_running_emits_idle_notice() {
        use crate::agent::model::RuntimeSessionState;
        let mut app = App::test_default();
        app.set_runtime_session_state(Some(RuntimeSessionState::Running));

        let consumed = try_handle_submit(&mut app, "/account");

        assert!(consumed, "/account is handled locally");
        assert!(app.account_picker.is_none(), "no picker opens while a turn is in flight");
        let last = app.messages().last().expect("a system notice");
        assert!(matches!(last.role, MessageRole::System(_)));
        let text: String = last
            .blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::Text(t) => Some(t.markdown.full_text()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("Finish or cancel"),
            "the notice names the idle requirement; got: {text}",
        );
    }

    #[test]
    fn account_command_passes_idle_gate_when_idle() {
        use crate::agent::model::RuntimeSessionState;
        let mut app = App::test_default();
        app.set_runtime_session_state(Some(RuntimeSessionState::Idle));

        let consumed = try_handle_submit(&mut app, "/account");

        assert!(consumed);
        // With no project/accounts wired into the test App the picker
        // can't populate, but the mid-turn notice is NOT what an idle
        // session hits - proving the gate distinguishes Idle from Running.
        let text: String = app
            .messages()
            .last()
            .map(|m| {
                m.blocks
                    .iter()
                    .filter_map(|b| match b {
                        MessageBlock::Text(t) => Some(t.markdown.full_text()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !text.contains("Finish or cancel"),
            "an idle session does not hit the mid-turn gate; got: {text}",
        );
    }

    #[test]
    fn account_command_allows_when_runtime_state_unknown() {
        // A freshly-connected session has no state message yet (None).
        // The open-gate must NOT false-refuse it - only a known Running /
        // RequiresAction turn is blocked (the workspace backstop is
        // authoritative).
        let mut app = App::test_default();
        app.set_runtime_session_state(None);

        let consumed = try_handle_submit(&mut app, "/account");

        assert!(consumed);
        let text: String = app
            .messages()
            .last()
            .map(|m| {
                m.blocks
                    .iter()
                    .filter_map(|b| match b {
                        MessageBlock::Text(t) => Some(t.markdown.full_text()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !text.contains("Finish or cancel"),
            "a None (freshly-connected) session is not turn-blocked; got: {text}",
        );
    }

    #[test]
    fn advertised_command_is_forwarded() {
        let mut app = App::test_default();
        app.try_active_bucket_mut().unwrap().available_commands =
            vec![model::AvailableCommand::new("/help", "Help")];
        let consumed = try_handle_submit(&mut app, "/help");
        assert!(!consumed);
    }

    #[test]
    fn builtin_commands_appear_in_candidates() {
        let app = App::test_default();
        let names: Vec<String> =
            supported_command_candidates(&app).into_iter().map(|c| c.primary).collect();
        for expected in
            ["/compact", "/effort", "/mcp", "/mode", "/model", "/new", "/plugins", "/resume"]
        {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        for removed in ["/1m-context", "/cancel", "/docs", "/login", "/logout", "/opus-version"] {
            assert!(!names.iter().any(|n| n == removed), "{removed} should be removed");
        }
    }

    #[test]
    fn plugins_without_args_opens_plugins_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        app.settings_home_override = Some(dir.path().to_path_buf());

        let consumed = try_handle_submit(&mut app, "/plugins");

        assert!(consumed);
        assert_eq!(app.active_view, super::super::ActiveView::Plugins);
    }

    #[test]
    fn mcp_opens_mcp_screen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        app.settings_home_override = Some(dir.path().to_path_buf());

        let consumed = try_handle_submit(&mut app, "/mcp");

        assert!(consumed);
        assert_eq!(app.active_view, super::super::ActiveView::Mcp);
    }

    #[test]
    fn mcp_with_extra_args_returns_usage() {
        let mut app = App::test_default();

        let consumed = try_handle_submit(&mut app, "/mcp extra");

        assert!(consumed);
        let Some(last) = app.messages().last() else {
            panic!("expected usage message");
        };
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /mcp");
    }

    #[test]
    fn plugins_with_extra_args_returns_usage() {
        let mut app = App::test_default();
        let dir = tempfile::tempdir().expect("tempdir");
        app.settings_home_override = Some(dir.path().to_path_buf());

        let consumed = try_handle_submit(&mut app, "/plugins extra");

        assert!(consumed);
        let Some(last) = app.messages().last() else {
            panic!("expected usage message");
        };
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /plugins");
    }

    #[test]
    fn detect_slash_argument_context_after_first_space() {
        let lines = vec!["/mode pla".to_owned()];
        let detection = detect_slash_at_cursor(&lines, 0, "/mode pla".chars().count())
            .expect("slash detection");

        match detection.context {
            SlashContext::Argument { command, arg_index, token_range } => {
                assert_eq!(command, "/mode");
                assert_eq!(arg_index, 0);
                assert_eq!(token_range, (6, 9));
            }
            SlashContext::CommandName => panic!("expected argument context"),
        }
        assert_eq!(detection.query, "pla");
    }

    #[test]
    fn mode_argument_candidates_are_dynamic() {
        let mut app = App::test_default();
        app.set_mode(Some(super::super::ModeState {
            current_mode_id: "plan".to_owned(),
            current_mode_name: "Plan".to_owned(),
            available_modes: vec![
                super::super::ModeInfo {
                    id: "plan".to_owned(),
                    name: "Plan".to_owned(),
                    description: None,
                },
                super::super::ModeInfo {
                    id: "code".to_owned(),
                    name: "Code".to_owned(),
                    description: None,
                },
            ],
        }));

        let candidates = argument_candidates(&app, "/mode", 0);
        assert!(candidates.iter().any(|c| c.insert_value == "plan"));
        assert!(candidates.iter().any(|c| c.insert_value == "code"));
        assert!(candidates.iter().any(|c| c.primary == "Plan"));
        assert!(candidates.iter().any(|c| c.secondary.as_deref() == Some("plan")));
    }

    #[test]
    fn model_argument_candidates_are_dynamic() {
        let mut app = App::test_default();
        app.try_active_bucket_mut().unwrap().available_models = vec![
            crate::agent::model::AvailableModel::new("sonnet", "Claude Sonnet")
                .description("Balanced coding model"),
            crate::agent::model::AvailableModel::new("opus", "Claude Opus"),
        ];
        let candidates = argument_candidates(&app, "/model", 0);
        assert!(candidates.iter().any(|c| c.insert_value == "sonnet"));
        assert!(candidates.iter().any(|c| c.primary == "Claude Sonnet"));
        assert!(candidates.iter().any(|c| c.secondary.as_deref() == Some("Balanced coding model")));
        assert!(candidates.iter().any(|c| c.insert_value == "opus"));
    }

    #[test]
    fn model_argument_candidates_hide_sdk_default_option() {
        let mut app = App::test_default();
        app.try_active_bucket_mut().unwrap().available_models = vec![
            crate::agent::model::AvailableModel::new("default", "Default")
                .description("Default (recommended)"),
            crate::agent::model::AvailableModel::new("sonnet", "Claude Sonnet"),
            crate::agent::model::AvailableModel::new("opus", "Claude Opus"),
        ];

        let candidates = argument_candidates(&app, "/model", 0);

        assert!(!candidates.iter().any(|c| c.insert_value == "default"));
        assert!(!candidates.iter().any(|c| c.primary == "Default"));
        assert!(candidates.iter().any(|c| c.insert_value == "sonnet"));
        assert!(candidates.iter().any(|c| c.insert_value == "opus"));
    }

    #[test]
    fn model_argument_candidates_rewrite_opus_secondary_from_project_pin() {
        let mut app = App::test_default();
        app.config.committed_local_settings_document = json!({
            "env": {
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-5-20251101"
            }
        });
        app.try_active_bucket_mut().unwrap().available_models = vec![
            crate::agent::model::AvailableModel::new("opus", "Opus")
                .description("Opus 4.7 · Most capable for complex work"),
        ];

        let candidates = argument_candidates(&app, "/model", 0);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].insert_value, "opus");
        assert_eq!(
            candidates[0].secondary.as_deref(),
            Some("Opus 4.5 · Most capable for complex work")
        );
    }

    #[test]
    fn model_argument_candidates_keep_sdk_opus_description_when_unpinned() {
        let mut app = App::test_default();
        app.try_active_bucket_mut().unwrap().available_models = vec![
            crate::agent::model::AvailableModel::new("opus", "Opus")
                .description("Opus 4.7 · Most capable for complex work"),
        ];

        let candidates = argument_candidates(&app, "/model", 0);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].insert_value, "opus");
        assert_eq!(
            candidates[0].secondary.as_deref(),
            Some("Opus 4.7 · Most capable for complex work")
        );
    }

    #[test]
    fn non_variable_command_argument_mode_is_disabled() {
        let mut app = App::test_default();
        app.input_mut().set_text("/compact now");
        let _ = app.input_mut().set_cursor(0, "/compact now".chars().count());
        sync_with_cursor(&mut app);
        assert!(app.slash().is_none());
    }

    #[test]
    fn variable_command_argument_mode_deactivates_when_no_match() {
        let mut app = App::test_default();
        app.set_mode(Some(super::super::ModeState {
            current_mode_id: "plan".to_owned(),
            current_mode_name: "Plan".to_owned(),
            available_modes: vec![super::super::ModeInfo {
                id: "plan".to_owned(),
                name: "Plan".to_owned(),
                description: None,
            }],
        }));
        app.input_mut().set_text("/mode xyz");
        let _ = app.input_mut().set_cursor(0, "/mode xyz".chars().count());
        sync_with_cursor(&mut app);
        assert!(app.slash().is_none());
    }

    #[test]
    fn confirm_selection_replaces_only_active_argument_token() {
        let mut app = App::test_default();
        app.input_mut().set_text("/resume old-id trailing");
        let _ = app.input_mut().set_cursor(0, "/resume old-id".chars().count());
        *app.slash_mut() = Some(SlashState {
            trigger_row: 0,
            trigger_col: 8,
            query: "old-id".to_owned(),
            context: SlashContext::Argument {
                command: "/resume".to_owned(),
                arg_index: 0,
                token_range: (8, 14),
            },
            candidates: vec![SlashCandidate {
                insert_value: "new-id".to_owned(),
                primary: "New".to_owned(),
                secondary: None,
            }],
            dialog: DialogState::default(),
        });

        confirm_selection(&mut app);

        assert_eq!(app.input().text(), "/resume new-id trailing");
    }

    #[test]
    fn resume_with_missing_id_returns_usage() {
        let mut app = App::test_default();
        let consumed = try_handle_submit(&mut app, "/resume");
        assert!(consumed);
        let Some(last) = app.messages().last() else {
            panic!("expected usage message");
        };
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /resume <session_id>");
    }

    #[test]
    fn resume_with_extra_args_returns_usage() {
        let mut app = App::test_default();
        let consumed = try_handle_submit(&mut app, "/resume abc-123 extra");
        assert!(consumed);
        let Some(last) = app.messages().last() else {
            panic!("expected usage message");
        };
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /resume <session_id>");
    }

    #[test]
    fn resume_command_is_rendered_as_user_message() {
        let mut app = App::test_default();

        let consumed = try_handle_submit(&mut app, "/resume abc-123");
        assert!(consumed);
        assert!(app.messages().len() >= 2);

        let Some(first) = app.messages().first() else {
            panic!("expected user message");
        };
        assert!(matches!(first.role, MessageRole::User));
        let Some(MessageBlock::Text(block)) = first.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "/resume abc-123");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_sets_command_pending_when_connected() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let mut rx = app.install_testing_stub();

                let consumed = try_handle_submit(&mut app, "/resume abc-123");
                assert!(consumed);
                assert!(matches!(app.status, AppStatus::CommandPending));
                assert_eq!(app.resuming_session_id(), Some("abc-123"));

                tokio::task::yield_now().await;
                let cmd = rx.try_recv().expect("resume command dispatched");
                assert!(matches!(
                    cmd,
                    forge_primitives::AgentCommand::ResumeSession { session_id, .. }
                        if session_id == "abc-123"
                ));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mode_apply_synchronously_during_submit() {
        // /mode applies CurrentModeUpdate + ModeStateUpdate optimistically
        // App-side. The apply is synchronous, so no CommandPending
        // state is needed.
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                app.set_session_id(Some("sess-1".into()));
                app.set_mode(Some(super::super::ModeState {
                    current_mode_id: "code".to_owned(),
                    current_mode_name: "Code".to_owned(),
                    available_modes: vec![
                        super::super::ModeInfo {
                            id: "plan".to_owned(),
                            name: "Plan".to_owned(),
                            description: None,
                        },
                        super::super::ModeInfo {
                            id: "code".to_owned(),
                            name: "Code".to_owned(),
                            description: None,
                        },
                    ],
                }));

                let consumed = try_handle_submit(&mut app, "/mode plan");
                assert!(consumed);
                assert_eq!(
                    app.mode().map(|m| m.current_mode_id.as_str()),
                    Some("plan"),
                    "expected mode applied synchronously to plan"
                );
                assert!(app.pending_command_label().is_none());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mode_bypass_dispatches_like_the_other_modes() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let mut rx = app.install_testing_stub();
                app.set_session_id(Some("sess-1".into()));
                // The candidate list is the real producer's output for a
                // bypass-launched session, not a hand-written list.
                let supported =
                    forge_workspace::commands::supported_mode_ids_filtered(false, true, None, &[]);
                app.set_mode(Some(forge_workspace::commands::build_mode_state_from_supported(
                    forge_workspace::PermissionMode::Ask,
                    &supported,
                )));

                let consumed = try_handle_submit(&mut app, "/mode bypassPermissions");
                assert!(consumed);
                assert_eq!(
                    app.mode().map(|m| m.current_mode_id.as_str()),
                    Some("bypassPermissions"),
                    "bypass applies synchronously like the other modes",
                );
                tokio::task::yield_now().await;
                let cmd = rx.try_recv().expect("SetMode dispatched");
                assert!(
                    matches!(
                        cmd,
                        forge_primitives::AgentCommand::SetMode { ref session_id, mode }
                            if session_id.as_str() == "sess-1"
                                && mode
                                    == forge_primitives::permission::PermissionMode::BypassPermissions
                    ),
                    "bypass dispatches the SetMode the other modes dispatch: {cmd:?}",
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mode_switching_away_keeps_bypass_offered() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                app.set_session_id(Some("sess-1".into()));
                // A bypass-launched session sitting in bypass.
                let supported =
                    forge_workspace::commands::supported_mode_ids_filtered(false, true, None, &[]);
                app.set_mode(Some(forge_workspace::commands::build_mode_state_from_supported(
                    forge_workspace::PermissionMode::BypassPermissions,
                    &supported,
                )));

                let consumed = try_handle_submit(&mut app, "/mode plan");
                assert!(consumed);
                assert_eq!(
                    app.mode().map(|m| m.current_mode_id.as_str()),
                    Some("plan"),
                    "optimistic away-leg applies the switch to plan",
                );
                let still_offered = app
                    .mode()
                    .is_some_and(|m| m.available_modes.iter().any(|e| e.id == "bypassPermissions"));
                assert!(still_offered, "switching away keeps bypass in the picker list");
            })
            .await;
    }

    fn seed_ask_session(app: &mut App) {
        app.set_session_id(Some("sess-1".into()));
        let supported =
            forge_workspace::commands::supported_mode_ids_filtered(false, true, None, &[]);
        app.set_mode(Some(forge_workspace::commands::build_mode_state_from_supported(
            forge_workspace::PermissionMode::Ask,
            &supported,
        )));
        app.with_turn_state_mut(|ts| ts.mode = Some(forge_workspace::PermissionMode::Ask));
    }

    fn first_block_text(msg: &ChatMessage) -> String {
        match msg.blocks.first() {
            Some(MessageBlock::Text(block)) => block.text.clone(),
            _ => panic!("expected a text block in the rejection message"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejected_set_mode_rolls_back_chip_and_pushes_message() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                seed_ask_session(&mut app);
                let seeded_supported = app.with_turn_state(|ts| ts.supported_mode_ids.clone());

                let consumed = try_handle_submit(&mut app, "/mode plan");
                assert!(consumed);
                assert_eq!(
                    app.mode().map(|m| m.current_mode_id.as_str()),
                    Some("plan"),
                    "optimistic apply flipped the chip before the rejection lands",
                );

                let key = app.active_session_key.clone().expect("test bucket key");
                crate::app::events::apply_session_update(
                    &mut app,
                    forge_workspace::SessionUpdate::SetModeFailed {
                        key,
                        mode: forge_primitives::permission::PermissionMode::Plan,
                        message: "mode not permitted".into(),
                    },
                );

                assert_eq!(
                    app.mode().map(|m| m.current_mode_id.as_str()),
                    Some("default"),
                    "rejection must roll the chip back to the pre-apply mode",
                );
                assert_eq!(
                    app.with_turn_state(|ts| ts.mode),
                    Some(forge_workspace::PermissionMode::Ask),
                    "rejection must restore the pre-apply typed turn-state mode",
                );
                assert_eq!(
                    app.with_turn_state(|ts| ts.supported_mode_ids.clone()),
                    seeded_supported,
                    "rejection must restore the pre-apply supported-mode list",
                );
                let last = app.messages().last().expect("rejection message pushed");
                assert!(
                    matches!(last.role, MessageRole::System(None)),
                    "rejection surfaces as a system message, got {:?}",
                    last.role
                );
                let text = first_block_text(last);
                assert!(text.contains("plan"), "message names the refused mode: {text}");
                assert!(
                    text.contains("mode not permitted"),
                    "CLI rejection text reaches the chat: {text}"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accepted_set_mode_keeps_optimistic_apply_and_no_error_message() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let mut rx = app.install_testing_stub();
                seed_ask_session(&mut app);
                let messages_before = app.messages().len();

                let consumed = try_handle_submit(&mut app, "/mode plan");
                assert!(consumed);
                assert_eq!(
                    app.mode().map(|m| m.current_mode_id.as_str()),
                    Some("plan"),
                    "an accepted switch keeps the optimistic apply",
                );
                tokio::task::yield_now().await;
                assert!(
                    matches!(rx.try_recv(), Ok(forge_primitives::AgentCommand::SetMode { .. })),
                    "success still dispatches SetMode",
                );
                assert_eq!(
                    app.messages().len(),
                    messages_before,
                    "no rejection message on the success path",
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_mode_rejection_does_not_consume_the_live_rollback() {
        // Two rapid submits overlap: the first refusal must leave the
        // newer optimistic apply (and its rollback snapshot) alone, and
        // the second refusal must restore the true pre-apply state so
        // the chip never shows a doubly-refused mode.
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                seed_ask_session(&mut app);

                assert!(try_handle_submit(&mut app, "/mode plan"));
                assert!(try_handle_submit(&mut app, "/mode acceptEdits"));
                assert_eq!(app.mode().map(|m| m.current_mode_id.as_str()), Some("acceptEdits"));

                let key = app.active_session_key.clone().expect("test bucket key");
                let refused = |mode| forge_workspace::SessionUpdate::SetModeFailed {
                    key: key.clone(),
                    mode,
                    message: "refused".into(),
                };
                crate::app::events::apply_session_update(
                    &mut app,
                    refused(forge_primitives::permission::PermissionMode::Plan),
                );
                assert_eq!(
                    app.mode().map(|m| m.current_mode_id.as_str()),
                    Some("acceptEdits"),
                    "a refusal for a superseded request must not roll the chip back",
                );

                crate::app::events::apply_session_update(
                    &mut app,
                    refused(forge_primitives::permission::PermissionMode::AcceptEdits),
                );
                assert_eq!(
                    app.mode().map(|m| m.current_mode_id.as_str()),
                    Some("default"),
                    "the chip must restore the pre-apply mode, not a refused one",
                );
                assert_eq!(
                    app.with_turn_state(|ts| ts.mode),
                    Some(forge_workspace::PermissionMode::Ask),
                    "the typed turn-state mode restores with the chip",
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confirmed_mode_clears_the_rollback_snapshot() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                seed_ask_session(&mut app);
                assert!(try_handle_submit(&mut app, "/mode plan"));

                let session_id = app.session_id().unwrap_or_default().to_string();
                crate::app::events::apply_session_update(
                    &mut app,
                    forge_workspace::SessionUpdate::ChatAppended {
                        session_id,
                        msg: forge_primitives::Message::System {
                            subtype: "status".into(),
                            data: serde_json::json!({"permissionMode": "plan"}),
                            session_id: None,
                        },
                    },
                );

                assert!(
                    app.pending_mode_rollback().is_none(),
                    "a CLI-confirmed mode retires the pending rollback",
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn model_apply_synchronously_during_submit() {
        // /model applies CurrentModelUpdate optimistically App-side.
        // The apply is synchronous, so no CommandPending state is
        // needed.
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                app.set_session_id(Some("sess-1".into()));
                app.set_current_model(Some(
                    crate::agent::model::CurrentModel::new("old-model", "old-model", "old-model")
                        .authoritative(true),
                ));

                let consumed = try_handle_submit(&mut app, "/model sonnet");
                assert!(consumed);
                assert_eq!(
                    app.current_model().map(|m| m.resolved_id.as_str()),
                    Some("sonnet"),
                    "expected current_model applied synchronously to sonnet"
                );
                assert!(app.pending_command_label().is_none());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_sets_command_pending() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();

                let consumed = try_handle_submit(&mut app, "/new");
                assert!(consumed);
                assert!(
                    matches!(app.status, AppStatus::CommandPending),
                    "expected CommandPending, got {:?}",
                    app.status
                );
                assert_eq!(app.pending_command_label(), Some("Starting new session..."));
            })
            .await;
    }

    #[test]
    fn compact_without_connection_is_handled_locally() {
        let mut app = App::test_default();

        let consumed = try_handle_submit(&mut app, "/compact");
        assert!(consumed);
        assert!(!app.pending_compact_clear());
        let Some(last) = app.messages().last() else {
            panic!("expected system message");
        };
        assert!(matches!(last.role, MessageRole::System(_)));
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Cannot compact: not connected yet.");
    }

    #[test]
    fn compact_with_active_session_falls_through_without_touching_state() {
        // `/compact` is wire-driven: the CLI emits `status:"compacting"`
        // as the first response frame, which `apply_session_status_update`
        // translates into `is_compacting = true`. The slash handler
        // returns `false` so `/compact` flows through as a regular
        // prompt; it does NOT optimistically set state.
        let mut app = App::test_default();
        let _rx = app.install_testing_stub();
        app.set_session_id(Some(model::SessionId::new("session-1")));

        let consumed = try_handle_submit(&mut app, "/compact");
        assert!(!consumed);
        assert!(!app.pending_compact_clear());
        assert!(
            !app.is_compacting(),
            "slash handler must not optimistically set is_compacting; wire status drives it"
        );
    }

    #[test]
    fn compact_with_args_returns_usage_message() {
        let mut app = App::test_default();
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete("keep"))],
        ));

        let consumed = try_handle_submit(&mut app, "/compact now");
        assert!(consumed);
        assert!(app.messages().len() >= 2);
        let Some(last) = app.messages().last() else {
            panic!("expected system usage message");
        };
        assert!(matches!(last.role, MessageRole::System(_)));
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /compact");
    }

    #[test]
    fn mode_with_extra_args_returns_usage_message() {
        let mut app = App::test_default();

        let consumed = try_handle_submit(&mut app, "/mode plan extra");
        assert!(consumed);
        let Some(last) = app.messages().last() else {
            panic!("expected system usage message");
        };
        assert!(matches!(last.role, MessageRole::System(_)));
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /mode [id]");
    }

    #[test]
    fn model_with_no_arg_reports_current_model() {
        let mut app = App::test_default();
        app.set_current_model(Some(
            crate::agent::model::CurrentModel::new("opus", "Opus", "Opus 4.7").authoritative(true),
        ));

        let consumed = try_handle_submit(&mut app, "/model");
        assert!(consumed);
        let Some(last) = app.messages().last() else {
            panic!("expected system message");
        };
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert!(block.text.starts_with("Model: "), "got `{}`", block.text);
        assert!(block.text.contains("Opus"));
    }

    #[test]
    fn model_with_extra_args_returns_usage_message() {
        let mut app = App::test_default();

        let consumed = try_handle_submit(&mut app, "/model sonnet extra");
        assert!(consumed);
        let Some(last) = app.messages().last() else {
            panic!("expected system usage message");
        };
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /model [id]");
    }

    #[test]
    fn confirm_selection_with_invalid_trigger_row_is_noop() {
        let mut app = App::test_default();
        app.input_mut().set_text("/mode");
        *app.slash_mut() = Some(SlashState {
            trigger_row: 99,
            trigger_col: 0,
            query: "m".into(),
            context: SlashContext::CommandName,
            candidates: vec![SlashCandidate {
                insert_value: "/mode".into(),
                primary: "/mode".into(),
                secondary: None,
            }],
            dialog: DialogState::default(),
        });

        confirm_selection(&mut app);

        assert_eq!(app.input().text(), "/mode");
    }

    #[test]
    fn mcp_appears_in_candidates() {
        let app = App::test_default();
        let names: Vec<String> =
            supported_command_candidates(&app).into_iter().map(|c| c.primary).collect();
        assert!(names.iter().any(|n| n == "/mcp"), "missing /mcp");
    }
}
