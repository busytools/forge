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

use forge_workspace::translate::commands::slash_name as normalize_slash_name;

pub(crate) fn push_system_message(app: &mut App, text: impl Into<String>) {
    let text = text.into();
    app.push_message_tracked(ChatMessage::new(
        MessageRole::System(None),
        vec![MessageBlock::Text(TextBlock::from_complete(&text))],
    ));
    app.enforce_history_retention_tracked();
    if let Some(viewport) = app.active_viewport_mut() {
        viewport.engage_auto_scroll();
    }
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
    if let Some(viewport) = app.active_viewport_mut() {
        viewport.engage_auto_scroll();
    }
}

fn push_user_message(app: &mut App, text: impl Into<String>) {
    let text = text.into();
    app.push_message_tracked(ChatMessage::new(
        MessageRole::User,
        vec![MessageBlock::Text(TextBlock::from_complete(&text))],
    ));
    app.enforce_history_retention_tracked();
    if let Some(viewport) = app.active_viewport_mut() {
        viewport.engage_auto_scroll();
    }
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
    if let Some(slot) = app.pending_command_label_mut() {
        *slot = Some(label.to_owned());
    }
    if let Some(slot) = app.pending_command_ack_mut() {
        *slot = ack;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, SystemSeverity};
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
        let Some(last) = app.messages().and_then(|messages| messages.last()) else {
            panic!("expected system message");
        };
        assert!(matches!(last.role, MessageRole::System(_)));
    }

    #[test]
    fn advertised_command_is_forwarded() {
        let mut app = App::test_default();
        app.active_bucket_mut().unwrap().available_commands =
            vec![model::AvailableCommand::new("/help", "Help")];
        let consumed = try_handle_submit(&mut app, "/help");
        assert!(!consumed);
    }

    #[test]
    fn builtin_commands_appear_in_candidates() {
        let app = App::test_default();
        let names: Vec<String> =
            supported_command_candidates(&app).into_iter().map(|c| c.primary).collect();
        for expected in ["/compact", "/effort", "/extensions", "/mode", "/model", "/new", "/resume"]
        {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        for removed in [
            "/1m-context",
            "/cancel",
            "/docs",
            "/login",
            "/logout",
            "/mcp",
            "/opus-version",
            "/plugins",
        ] {
            assert!(!names.iter().any(|n| n == removed), "{removed} should be removed");
        }
    }

    #[test]
    fn extensions_opens_the_extensions_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        app.settings_home_override = Some(dir.path().to_path_buf());

        let consumed = try_handle_submit(&mut app, "/extensions");

        assert!(consumed);
        assert_eq!(app.active_view, super::super::ActiveView::Extensions);
    }

    #[test]
    fn extensions_with_extra_args_returns_usage() {
        let mut app = App::test_default();

        let consumed = try_handle_submit(&mut app, "/extensions extra");

        assert!(consumed);
        let Some(last) = app.messages().and_then(|messages| messages.last()) else {
            panic!("expected usage message");
        };
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /extensions");
    }

    /// The retired commands are gone, not aliased.
    #[test]
    fn the_retired_plugins_and_mcp_commands_no_longer_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        app.settings_home_override = Some(dir.path().to_path_buf());

        assert!(!try_handle_submit(&mut app, "/plugins"), "/plugins is retired");
        assert!(!try_handle_submit(&mut app, "/mcp"), "/mcp is retired");
        assert_eq!(app.active_view, crate::app::ActiveView::Chat);
    }

    #[test]
    fn extensions_extra_args_returns_usage_from_the_page_open_gate() {
        let mut app = App::test_default();
        let dir = tempfile::tempdir().expect("tempdir");
        app.settings_home_override = Some(dir.path().to_path_buf());

        // The retired /plugins no longer routes; submitting it falls
        // through as an unknown command.
        assert!(!try_handle_submit(&mut app, "/plugins extra"));
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
        app.active_bucket_mut().unwrap().available_models = vec![
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
        app.active_bucket_mut().unwrap().available_models = vec![
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
        app.active_bucket_mut().unwrap().available_models = vec![
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
        app.active_bucket_mut().unwrap().available_models = vec![
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
        app.input_mut().expect("active session").set_text("/compact now");
        let _ =
            app.input_mut().expect("active session").set_cursor(0, "/compact now".chars().count());
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
        app.input_mut().expect("active session").set_text("/mode xyz");
        let _ = app.input_mut().expect("active session").set_cursor(0, "/mode xyz".chars().count());
        sync_with_cursor(&mut app);
        assert!(app.slash().is_none());
    }

    #[test]
    fn confirm_selection_replaces_only_active_argument_token() {
        let mut app = App::test_default();
        app.input_mut().expect("active session").set_text("/resume old-id trailing");
        let _ = app
            .input_mut()
            .expect("active session")
            .set_cursor(0, "/resume old-id".chars().count());
        *app.slash_mut().expect("active session") = Some(SlashState {
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

        assert_eq!(app.input().expect("active session").text(), "/resume new-id trailing");
    }

    /// `/resume <id>` is the core's own command, so this view draws the
    /// words and sends them, and blocks its input while the replacement
    /// lands. A view that refused the name would answer with a system
    /// message instead of the reader's own line.
    #[tokio::test(flavor = "current_thread")]
    async fn resume_is_drawn_and_forwarded_to_the_core() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();

                let consumed = try_handle_submit(&mut app, "/resume abc-123");

                assert!(consumed);
                assert!(matches!(app.status, AppStatus::CommandPending));
                let first = app.messages().and_then(|messages| messages.first());
                let Some(MessageBlock::Text(block)) = first.and_then(|m| m.blocks.first()) else {
                    panic!("expected the reader's own line first");
                };
                assert_eq!(block.text, "/resume abc-123");
            })
            .await;
    }

    /// Only the commands that replace the seat's occupant block the input,
    /// because the replacement arriving is what clears the row. A row nothing
    /// clears leaves the composer disabled for the rest of the session, which
    /// is what one typed `/mode plan` used to do.
    #[tokio::test(flavor = "current_thread")]
    async fn only_the_respawning_commands_block_the_input() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for text in ["/mode plan", "/model sonnet", "/effort high"] {
                    let mut app = App::test_default();
                    let _rx = app.install_testing_stub();

                    assert!(try_handle_submit(&mut app, text), "{text} is taken");

                    assert!(
                        !matches!(app.status, AppStatus::CommandPending),
                        "{text} answers, so the input must not stay blocked behind it",
                    );
                }

                for text in ["/new", "/resume abc-123"] {
                    let mut app = App::test_default();
                    let _rx = app.install_testing_stub();

                    assert!(try_handle_submit(&mut app, text), "{text} is taken");

                    assert!(
                        matches!(app.status, AppStatus::CommandPending),
                        "{text} replaces the occupant, so the input waits for it",
                    );
                }
            })
            .await;
    }

    /// A forge name invoked wrongly is still forge's, so this view forwards
    /// it and the core answers: deciding here is the second path the shared
    /// interception exists to remove.
    #[test]
    fn a_wrong_forge_invocation_is_forwarded_rather_than_decided_here() {
        for text in ["/resume", "/resume abc-123 extra", "/new session"] {
            let mut app = App::test_default();
            let consumed = try_handle_submit(&mut app, text);

            assert!(consumed, "{text} is taken by a view, never sent as a prompt");
            let last = app.messages().and_then(|messages| messages.last());
            assert!(
                matches!(last, Some(message) if message.role == MessageRole::User),
                "{text} is drawn as the reader's own line, so this view did not decide it",
            );
        }
    }

    /// The core's own line, which every forge command answers through. The
    /// info arm is the one `/effort` takes, and the error arm is what a
    /// refusal takes; a reducer that drew neither would pass every other test
    /// in this file.
    #[tokio::test(flavor = "current_thread")]
    async fn a_core_notice_is_drawn_at_the_severity_it_carries() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                let key = app.active_session_key.clone().expect("active session");

                crate::app::events::apply_session_update(
                    &mut app,
                    forge_workspace::SessionUpdate::Notice {
                        key: key.clone(),
                        severity: forge_workspace::NoticeSeverity::Info,
                        text: "Effort: High (takes effect next session)".into(),
                    },
                );
                let last = app.messages().and_then(|messages| messages.last());
                assert!(
                    matches!(last, Some(message)
                        if message.role == MessageRole::System(Some(SystemSeverity::Info))),
                    "an answer draws as an info message, got {:?}",
                    last.map(|message| &message.role),
                );
            })
            .await;
    }

    /// The same line for a seat nobody is looking at: it lands in that
    /// bucket rather than being dropped, so a reader who switches to it finds
    /// the answer waiting.
    #[tokio::test(flavor = "current_thread")]
    async fn a_core_notice_for_another_seat_lands_in_its_own_bucket() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                let elsewhere = forge_workspace::SessionSlot::from_str_for_test("other-seat");
                app.sessions.entry(elsewhere.clone()).or_insert_with(|| {
                    crate::app::session::UiSession::new(elsewhere.clone(), "other")
                });
                let shown_before = app.messages().map(<[_]>::len).unwrap_or_default();

                crate::app::events::apply_session_update(
                    &mut app,
                    forge_workspace::SessionUpdate::Notice {
                        key: elsewhere.clone(),
                        severity: forge_workspace::NoticeSeverity::Error,
                        text: "Usage: /mode <id>".into(),
                    },
                );

                let held = app.sessions.get(&elsewhere).expect("the bucket is held");
                assert!(
                    !held.messages.is_empty(),
                    "the line is held for the seat it was addressed to"
                );
                assert!(
                    matches!(
                        held.messages.last().map(|message| &message.role),
                        Some(MessageRole::System(None)),
                    ),
                    "an error reads as one rather than as an informational line, got {:?}",
                    held.messages.last().map(|message| &message.role),
                );
                assert_eq!(
                    app.messages().map(<[_]>::len).unwrap_or_default(),
                    shown_before,
                    "and the seat on screen is not drawn into",
                );
            })
            .await;
    }

    /// The core writes `settings.json` itself - `/effort` is its command now -
    /// so a line from it is the moment this view re-reads the documents its
    /// own spawns are built from. A snapshot taken at boot would otherwise
    /// launch the next session on the level before the change.
    ///
    /// **Two of the three documents, deliberately.** A home override bypasses
    /// the workspace bridge, and the user settings document has no path
    /// without one, so what this reaches is the preferences document and the
    /// project-local one. The settings half runs the same code on the same
    /// read and is the arm the bridge route covers.
    #[tokio::test(flavor = "current_thread")]
    async fn a_core_notice_re_reads_the_documents_a_launch_uses() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().expect("tempdir");
                std::fs::write(
                    dir.path().join(".claude.json"),
                    br#"{ "respectGitignore": false }"#,
                )
                .expect("seed the preferences document");
                let root = dir.path().join("project");
                std::fs::create_dir_all(root.join(".claude")).expect("mkdir the project");
                std::fs::write(
                    root.join(".claude").join("settings.local.json"),
                    br#"{ "outputStyle": "Learning" }"#,
                )
                .expect("seed the project-local document");
                let mut app = App::test_default();
                app.settings_home_override = Some(dir.path().to_path_buf());
                // The focused seat's cwd is what the project-local document is
                // read against, and the test App seeds one of its own.
                app.set_cwd_raw(root.to_string_lossy().into_owned());
                assert!(
                    app.config.committed_preferences_document.get("respectGitignore").is_none(),
                    "precondition: the held snapshot has not read it",
                );

                // A seat the terminal is NOT showing, which is where a client's
                // `/effort` lands: the re-read must not be behind the arm that
                // only the seat on screen reaches.
                let elsewhere = forge_workspace::SessionSlot::from_str_for_test("other-seat");
                app.sessions.entry(elsewhere.clone()).or_insert_with(|| {
                    crate::app::session::UiSession::new(elsewhere.clone(), "other")
                });
                crate::app::events::apply_session_update(
                    &mut app,
                    forge_workspace::SessionUpdate::Notice {
                        key: elsewhere,
                        severity: forge_workspace::NoticeSeverity::Info,
                        text: "Effort: High (takes effect next session)".into(),
                    },
                );

                assert_eq!(
                    app.config.committed_preferences_document.get("respectGitignore"),
                    Some(&json!(false)),
                    "the launch documents are re-read when the core answers",
                );
                assert_eq!(
                    app.config.committed_local_settings_document.get("outputStyle"),
                    Some(&json!("Learning")),
                    "and so is the project-local one",
                );
            })
            .await;
    }

    /// A refused mode change still lands as a system message, whichever view
    /// asked for it: this reducer is what the CLI's `SetModeFailed` becomes.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refused_set_mode_surfaces_as_a_system_message() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();

                let key = app.active_session_key.clone().expect("test bucket key");
                crate::app::events::apply_session_update(
                    &mut app,
                    forge_workspace::SessionUpdate::SetModeFailed {
                        key,
                        mode: forge_primitives::permission::PermissionMode::Plan,
                        message: "mode not permitted".into(),
                    },
                );

                let last = app
                    .messages()
                    .and_then(|messages| messages.last())
                    .expect("rejection message pushed");
                assert!(
                    matches!(last.role, MessageRole::System(None)),
                    "rejection surfaces as a system message, got {:?}",
                    last.role,
                );
                let Some(MessageBlock::Text(block)) = last.blocks.first() else {
                    panic!("expected a text block in the rejection message");
                };
                assert!(block.text.contains("plan"), "message names the refused mode");
                assert!(
                    block.text.contains("mode not permitted"),
                    "CLI rejection text reaches the chat",
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_model_picker_applies_its_choice_synchronously() {
        // The picker is the one place this view still picks a model, so the
        // optimistic apply it makes is what keeps the header honest until the
        // CLI confirms.
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();
                app.set_session_id(Some("sess-1".into()));
                app.set_current_model(Some(
                    crate::agent::model::CurrentModel::new("old-model", "old-model", "old-model")
                        .authoritative(true),
                ));
                let key = app.active_session_key.clone().expect("active session");

                switch_model(&mut app, key, "sonnet");

                assert_eq!(
                    app.current_model().map(|m| m.resolved_id.as_str()),
                    Some("sonnet"),
                    "expected current_model applied synchronously to sonnet"
                );
            })
            .await;
    }

    /// `/new` is the core's own command wherever it is typed: this view draws
    /// the words and sends them, and the core's interception restarts the
    /// seat. A view that refused the name would answer with a system message
    /// instead of the reader's own line.
    #[tokio::test(flavor = "current_thread")]
    async fn new_is_drawn_and_forwarded_to_the_core() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::test_default();
                let _rx = app.install_testing_stub();

                let consumed = try_handle_submit(&mut app, "/new");

                assert!(consumed);
                let last = app.messages().and_then(|messages| messages.last());
                assert!(
                    matches!(last, Some(message) if message.role == MessageRole::User),
                    "the reader's own line is drawn, so the core's command is not refused here",
                );
            })
            .await;
    }

    #[test]
    fn compact_without_connection_is_handled_locally() {
        let mut app = App::test_default();

        let consumed = try_handle_submit(&mut app, "/compact");
        assert!(consumed);
        assert!(!app.pending_compact_clear());
        let Some(last) = app.messages().and_then(|messages| messages.last()) else {
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
        app.active_messages_mut().expect("active session").push(ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete("keep"))],
        ));

        let consumed = try_handle_submit(&mut app, "/compact now");
        assert!(consumed);
        assert!(app.messages().expect("active session").len() >= 2);
        let Some(last) = app.messages().and_then(|messages| messages.last()) else {
            panic!("expected system usage message");
        };
        assert!(matches!(last.role, MessageRole::System(_)));
        let Some(MessageBlock::Text(block)) = last.blocks.first() else {
            panic!("expected text block");
        };
        assert_eq!(block.text, "Usage: /compact");
    }

    /// With no rows to pick from, a bare `/model` is the core's: it answers,
    /// and a picker nobody could choose from does not open.
    #[test]
    fn model_with_no_models_falls_through_to_the_core() {
        let mut app = App::test_default();
        app.set_current_model(Some(
            crate::agent::model::CurrentModel::new("opus", "Opus", "Opus 4.7").authoritative(true),
        ));

        let consumed = try_handle_submit(&mut app, "/model");

        assert!(consumed);
        assert!(app.model_picker.is_none(), "there are no rows to choose from");
        let last = app.messages().and_then(|messages| messages.last());
        assert!(
            matches!(last, Some(message) if message.role == MessageRole::User),
            "the words go out to the core, which answers",
        );
    }

    #[test]
    fn confirm_selection_with_invalid_trigger_row_is_noop() {
        let mut app = App::test_default();
        app.input_mut().expect("active session").set_text("/mode");
        *app.slash_mut().expect("active session") = Some(SlashState {
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

        assert_eq!(app.input().expect("active session").text(), "/mode");
    }

    #[test]
    fn extensions_appears_in_candidates() {
        let app = App::test_default();
        let names: Vec<String> =
            supported_command_candidates(&app).into_iter().map(|c| c.primary).collect();
        assert!(names.iter().any(|n| n == "/extensions"), "missing /extensions");
    }
}
