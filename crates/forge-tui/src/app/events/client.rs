use std::time::Instant;

use super::{App, session, turn};
use forge_workspace::{SessionSlot, SessionUpdate};

/// Side-effects shared by `Connected` and `SessionReplaced`: refresh
/// MCP + status + oauth-credentials + context-usage snapshots, and
/// kick a usage poll so the Projects pane's 5h/7d bars land within
/// seconds of session start instead of staying on placeholder ` - %`.
///
/// Every one of these reads and writes through the active-session
/// accessors, so they run under an active-bucket pivot onto `key` -
/// the session that just connected, which is not necessarily the one
/// the user is looking at.
fn post_connect_refreshes(app: &mut App, key: &SessionSlot) {
    crate::app::active_bucket_scope::with_pivoted(app, key.clone(), |app| {
        crate::app::config::refresh_mcp_snapshot(app);
        crate::app::session_runtime::request_status_snapshot_refresh(app);
        crate::app::session_runtime::request_oauth_credentials_snapshot_refresh(app);
        crate::app::session_runtime::request_context_usage_refresh(app);
        crate::app::usage::request_refresh_if_needed(app);
    });
}

/// Apply `f` only when `cwd_raw` matches the app's current cwd; log
/// and drop the event otherwise, reporting the drop. Plugin
/// lifecycle events are cwd-scoped and stale ones from a previous
/// project must not affect the active project's inventory.
fn dispatch_if_cwd_matches(
    app: &mut App,
    cwd_raw: &str,
    event_name: &str,
    f: impl FnOnce(&mut App),
) -> bool {
    if app.cwd_raw().unwrap_or_default() == cwd_raw {
        f(app);
        true
    } else {
        tracing::debug!(
            target: crate::logging::targets::APP_CONFIG,
            event_name,
            expected_cwd = %app.cwd_raw().unwrap_or_default(),
            received_cwd = %cwd_raw,
            "stale-cwd plugin event dropped"
        );
        false
    }
}

/// Compact discriminant name for a wire `Message`. Used by the
/// `sdk_message_dropped` error log so a triage grep can see whether
/// the dropped envelope was a Result (TurnComplete carrier),
/// Assistant content, etc. - without dumping the full payload.
fn msg_variant_name(msg: &forge_primitives::Message) -> &'static str {
    match msg {
        forge_primitives::Message::Assistant { .. } => "Assistant",
        forge_primitives::Message::User { .. } => "User",
        forge_primitives::Message::System { .. } => "System",
        forge_primitives::Message::Result { .. } => "Result",
        forge_primitives::Message::TaskStarted { .. } => "TaskStarted",
        forge_primitives::Message::TaskUpdated { .. } => "TaskUpdated",
        forge_primitives::Message::TaskProgress { .. } => "TaskProgress",
        forge_primitives::Message::TaskNotification { .. } => "TaskNotification",
        forge_primitives::Message::ThinkingTokens { .. } => "ThinkingTokens",
        forge_primitives::Message::TurnDuration { .. } => "TurnDuration",
        forge_primitives::Message::StopHookSummary { .. } => "StopHookSummary",
        forge_primitives::Message::BackgroundTasksChanged { .. } => "BackgroundTasksChanged",
        forge_primitives::Message::CommandsChanged { .. } => "CommandsChanged",
        forge_primitives::Message::HookStarted { .. } => "HookStarted",
        forge_primitives::Message::HookProgress { .. } => "HookProgress",
        forge_primitives::Message::PermissionDenied { .. } => "PermissionDenied",
        forge_primitives::Message::HookResponse { .. } => "HookResponse",
        forge_primitives::Message::Notification { .. } => "Notification",
        forge_primitives::Message::CompactBoundary { .. } => "CompactBoundary",
        forge_primitives::Message::RateLimitEvent { .. } => "RateLimitEvent",
        forge_primitives::Message::StreamEvent { .. } => "StreamEvent",
        forge_primitives::Message::Error { .. } => "Error",
        forge_primitives::Message::Unknown { .. } => "Unknown",
    }
}

/// The id the bucket at `key` currently runs under, for the wire-shaped
/// frames the TUI forges for its own chat echo. Empty while the bucket
/// has not connected yet - the echo's `session_id` is a display field
/// nothing routes on.
fn bucket_session_id(app: &App, key: &SessionSlot) -> String {
    app.sessions
        .get(key)
        .and_then(|bucket| bucket.session_id.as_ref())
        .map_or_else(String::new, |id| id.as_str().to_owned())
}

/// Per-session event multiplexer. Each [`SessionUpdate`] is routed
/// to the [`crate::app::session::UiSession`] bucket it targets via the
/// envelope's [`SessionUpdate::slot`] accessor.
///
/// `needs_redraw` is flipped only when the routed event targets the
/// active session - background-session events update their bucket
/// silently. App-global events (no slot) flip the redraw
/// flag unconditionally because they affect the rendered view.
pub fn apply_session_update(app: &mut App, update: SessionUpdate) {
    // INVARIANT: `is_active_or_global` is captured BEFORE the match
    // so reducers that themselves mutate `active_session_key` (e.g.
    // `Connected`, `SessionReplaced`) must set `needs_redraw = true`
    // explicitly via their own side effects. The post-match flip
    // sees the pre-handler active_session_key.
    let target_key = update.slot().cloned();
    let is_active_or_global = match &target_key {
        Some(key) => app.active_session_key.as_ref() == Some(key),
        None => true,
    };
    // Polled events can arrive unchanged; their reducer clears this so a
    // no-op response doesn't wake the render loop.
    let mut redraw = is_active_or_global;
    match update {
        SessionUpdate::Spawning { key, project_name, cwd, display_name } => {
            apply_session_update_spawning(app, key, &project_name, &cwd, &display_name);
        }
        SessionUpdate::Connected {
            key,
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history,
            compaction_count,
        } => {
            session::apply_session_update_connected(
                app,
                &key,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                &history,
                compaction_count,
            );
            post_connect_refreshes(app, &key);
        }
        SessionUpdate::SessionReplaced {
            key,
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history,
            compaction_count,
        } => {
            session::apply_session_update_session_replaced(
                app,
                &key,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                &history,
                compaction_count,
            );
            post_connect_refreshes(app, &key);
        }
        SessionUpdate::SessionsListed { key, sessions } => {
            session::apply_session_update_sessions_listed(app, &key, sessions);
        }
        SessionUpdate::AuthRequired { key, method_name, method_description } => {
            session::apply_session_update_auth_required(app, &key, method_name, method_description);
        }
        SessionUpdate::ConnectionFailed { key, message, fatal } => {
            session::apply_session_update_connection_failed(app, &key, &message, fatal);
        }
        SessionUpdate::SlashCommandError { key, message } => {
            session::apply_session_update_slash_command_error(app, &key, &message);
        }
        SessionUpdate::SetModeFailed { key, mode, message } => {
            session::apply_session_update_set_mode_failed(app, &key, mode, &message);
        }
        SessionUpdate::SetModelFailed { key, model, message } => {
            session::apply_session_update_set_model_failed(app, &key, &model, &message);
        }
        SessionUpdate::ServiceStatus { severity, message } => {
            session::apply_session_update_service_status(app, severity, &message);
        }
        SessionUpdate::CatalogLoaded => {
            app.needs_redraw = true;
        }
        SessionUpdate::FatalError(error) => {
            session::apply_session_update_fatal_error(app, error);
        }
        SessionUpdate::ForgeAccountIdentity { key, display_name } => {
            apply_session_update_forge_account_identity(app, &key, display_name);
        }
        SessionUpdate::DictateOverrides { key, overrides } => {
            if let Some(bucket) = app.sessions.get_mut(&key) {
                bucket.dictate_overrides = overrides;
                app.needs_redraw = true;
            }
        }
        SessionUpdate::DictateDevicePin { pick, .. } => {
            // The pick is workspace state shared by every session; the
            // key in the echo is the dispatching session and nothing
            // more.
            app.dictate_device_pin = pick;
            app.needs_redraw = true;
        }
        SessionUpdate::StatusSnapshot { key, account, forge_account } => {
            apply_session_update_status_snapshot(app, &key, account, forge_account);
        }
        SessionUpdate::OauthCredentialsSnapshot { key, credentials } => {
            apply_session_update_oauth_credentials_snapshot(app, &key, credentials);
        }
        SessionUpdate::ContextUsageSnapshot { key, percentage, max_tokens } => {
            apply_session_update_context_usage_snapshot(app, &key, percentage, max_tokens);
        }
        SessionUpdate::McpSnapshot { key, servers, error } => {
            redraw &= apply_session_update_mcp_snapshot(app, &key, servers, error);
        }
        SessionUpdate::ChatAppended { key, msg } => {
            apply_session_update_chat_appended(app, &key, msg);
        }
        SessionUpdate::HookObservation {
            key,
            tool_use_id,
            permission_mode,
            effort,
            agent_id,
            agent_type,
        } => {
            apply_session_update_hook_observation(
                app,
                &key,
                tool_use_id.as_deref(),
                permission_mode.as_deref(),
                effort.as_deref(),
                agent_id.as_deref(),
                agent_type.as_deref(),
            );
        }
        SessionUpdate::RuntimeReloadCompleted { key } => {
            apply_session_update_runtime_reload_completed(app, &key);
        }
        SessionUpdate::RuntimeReloadFailed { key, message } => {
            apply_session_update_runtime_reload_failed(app, &key, &message);
        }
        SessionUpdate::SlackPostPending { key, draft } => {
            // Queued on the ASKING session, so the approval is answered by
            // whoever will read the reply rather than by whichever session
            // happens to be focused.
            let asking = key.clone();
            let mut queued = false;
            if let Some(session) = app.session_mut(&key) {
                let prompt = crate::app::prompt::PromptState::from_slack_draft(asking, draft);
                crate::app::prompt::enqueue_prompt(session, prompt);
                queued = true;
            }
            // The asking session's `slack__post` is blocked on this answer,
            // so a prompt that was silently dropped would hold it forever.
            if !queued {
                tracing::warn!(
                    target: crate::logging::targets::APP_PERMISSION,
                    slot = %key.display(),
                    "slack approval prompt dropped: no session bucket for the asking session",
                );
                return;
            }
            // A parked draft on an unfocused session is invisible unless
            // the user is pointed at it.
            app.notify(crate::app::notify::NotifyEvent::PermissionRequired, &key);
        }
        SessionUpdate::SlackDraftExpired { key, id } => {
            // The gate expired the draft unanswered, so nothing was sent:
            // retire the dock prompt instead of leaving a decision the
            // user's answer can no longer reach.
            if let Some(session) = app.session_mut(&key) {
                crate::app::prompt::retire_slack_draft(session, id);
            }
        }
        SessionUpdate::PermissionRequest { key, tool_id, request } => {
            let mut queued = false;
            if let Some(session) = app.session_mut(&key) {
                let prompt = crate::app::prompt::PromptState::from_permission(tool_id, request);
                crate::app::prompt::enqueue_prompt(session, prompt);
                queued = true;
            }
            crate::app::prompt::snapshot_draft_if_needed(app, &key);
            // Only once it is answerable: a prompt whose session has no
            // bucket was dropped, and pointing the user at it would send
            // them looking for something that is not there.
            if queued {
                app.notify(crate::app::notify::NotifyEvent::PermissionRequired, &key);
            }
        }
        SessionUpdate::QuestionRequest { key, tool_id, request } => {
            let mut queued = false;
            if let Some(session) = app.session_mut(&key) {
                let prompt = crate::app::prompt::PromptState::from_question(tool_id, request);
                crate::app::prompt::enqueue_prompt(session, prompt);
                queued = true;
            }
            crate::app::prompt::snapshot_draft_if_needed(app, &key);
            if queued {
                app.notify(crate::app::notify::NotifyEvent::QuestionRequired, &key);
            }
        }
        SessionUpdate::McpOperationError { key, error } => {
            crate::app::config::handle_mcp_operation_error(app, &key, &error);
        }
        SessionUpdate::TurnComplete { key, terminal_reason } => {
            turn::apply_session_update_turn_complete(app, &key, terminal_reason);
        }
        SessionUpdate::TurnCancelled { key } => {
            turn::apply_session_update_turn_cancelled(app, &key);
        }
        SessionUpdate::TurnError { key, message, class, terminal_reason } => {
            turn::apply_session_update_turn_error(app, &key, &message, class, terminal_reason);
        }
        SessionUpdate::PromptQueuedWhileBusy { key } => {
            // TurnComplete settles regardless; a queued turn
            // announces itself on the wire when it starts.
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "prompt_queued_while_busy",
                message = "a dispatch landed while the session's turn was in flight",
                outcome = "success",
                session_slot = %key.display(),
            );
        }
        SessionUpdate::PluginsInventoryUpdated { cwd_raw, snapshot, claude_path } => {
            let applied =
                dispatch_if_cwd_matches(app, &cwd_raw, "plugins_inventory_dropped", |app| {
                    crate::app::extensions::apply_inventory_refresh_success(
                        app,
                        snapshot,
                        claude_path,
                    );
                });
            if !applied {
                crate::app::extensions::settle_dropped_refresh_failure(app);
            }
        }
        SessionUpdate::PluginsInventoryRefreshFailed { cwd_raw, message, trigger } => {
            // A boot auto-update run is app-scoped (see the run arms
            // below): its failure must unpin the seeded run whatever
            // session holds the focus.
            if trigger == forge_primitives::plugins::PluginUpdateTrigger::Auto {
                crate::app::extensions::apply_inventory_refresh_failure(app, message);
            } else {
                let applied = dispatch_if_cwd_matches(
                    app,
                    &cwd_raw,
                    "plugins_inventory_failure_dropped",
                    |app| {
                        crate::app::extensions::apply_inventory_refresh_failure(app, message);
                    },
                );
                if !applied {
                    crate::app::extensions::settle_dropped_refresh_failure(app);
                }
            }
        }
        SessionUpdate::PluginsCliActionSucceeded { cwd_raw, result } => {
            dispatch_if_cwd_matches(app, &cwd_raw, "plugins_cli_success_dropped", |app| {
                crate::app::extensions::apply_cli_action_success(app, result);
            });
        }
        SessionUpdate::PluginsCliActionFailed { cwd_raw, message } => {
            dispatch_if_cwd_matches(app, &cwd_raw, "plugins_cli_failure_dropped", |app| {
                crate::app::extensions::apply_cli_action_failure(app, message);
            });
        }
        SessionUpdate::PluginsUpdateRunProgress { cwd_raw, run } => {
            // A boot auto-update run is app-scoped: it borrows a
            // project cwd that need not match the focused session, so
            // its events bypass the cwd gate.
            if run.trigger == forge_primitives::plugins::PluginUpdateTrigger::Auto {
                crate::app::extensions::apply_update_run_progress(app, run);
            } else {
                dispatch_if_cwd_matches(
                    app,
                    &cwd_raw,
                    "plugins_update_run_progress_dropped",
                    |app| {
                        crate::app::extensions::apply_update_run_progress(app, run);
                    },
                );
            }
        }
        SessionUpdate::PluginsUpdateRunFinished { cwd_raw, run, snapshot, claude_path } => {
            if run.trigger == forge_primitives::plugins::PluginUpdateTrigger::Auto {
                crate::app::extensions::apply_update_run_finished(app, &run, snapshot, claude_path);
            } else {
                let applied = dispatch_if_cwd_matches(
                    app,
                    &cwd_raw,
                    "plugins_update_run_finished_dropped",
                    |app| {
                        crate::app::extensions::apply_update_run_finished(
                            app,
                            &run,
                            snapshot,
                            claude_path,
                        );
                    },
                );
                if !applied {
                    crate::app::extensions::settle_dropped_manual_run(app);
                }
            }
        }
        SessionUpdate::PluginsRollbackSucceeded {
            cwd_raw,
            plugin_id,
            scope,
            message,
            snapshot,
            claude_path,
        } => {
            dispatch_if_cwd_matches(app, &cwd_raw, "plugins_rollback_success_dropped", |app| {
                crate::app::extensions::apply_rollback_success(
                    app,
                    &plugin_id,
                    &scope,
                    message,
                    snapshot,
                    claude_path,
                );
            });
        }
        SessionUpdate::PluginsRollbackFailed { cwd_raw, plugin_id, message, snapshot } => {
            dispatch_if_cwd_matches(app, &cwd_raw, "plugins_rollback_failure_dropped", |app| {
                crate::app::extensions::apply_rollback_failure(app, &plugin_id, &message, snapshot);
            });
        }
        SessionUpdate::PeerInflightStatsChanged { key, stats } => {
            if let Some(session) = app.session_mut(&key) {
                // Track when the failure counters last incremented so
                // the projects_pane render can fade those indicators
                // after 60 s.
                if stats.delivery_failed > session.peer_badges.delivery_failed {
                    session.peer_badges_last_failure_at = Some(std::time::Instant::now());
                }
                session.peer_badges = stats;
            }
        }
        SessionUpdate::ReviewActivityNotice { key, branch, waiting, message } => {
            // A worker's review turn ended; drop the batched tally into the
            // reviewer's (submit-origin) session chat. No-op if that session
            // isn't live here.
            if let Some(session) = app.session_mut(&key) {
                // The chat line scrolls away, so park the count too - that
                // is what the GIT badge and the attention band render from.
                session.review_replies_waiting = crate::app::ReviewRepliesWaiting::merge(
                    session.review_replies_waiting.as_ref(),
                    &branch,
                    waiting,
                );
            }
            super::push_system_message_to_session(
                app,
                &key,
                Some(crate::app::SystemSeverity::Info),
                &message,
            );
        }
        SessionUpdate::WorkerStatusChanged { action, status, worktree, .. } => {
            // Workspace owns the authoritative live_workers map; the
            // projects-pane renderer reads from `workspace.list_live_workers`
            // each frame so a redraw covers Added / StatusChanged.
            // Removed additionally surfaces a system-message toast in
            // the worker's spawning-lead session (not the focused one,
            // so a despawn can't leak its toast across projects;
            // dropped when that lead isn't live here) so the operator
            // knows the worker is gone and what became of its worktree.
            // Removed ALSO drops the worker's UiSession bucket from
            // `app.sessions` and, when it was the active session, falls
            // back to the worker's spawning lead (the row its subtree
            // hangs off) or else to the row the pane drew next to it -
            // without this cleanup the bucket lingers with stale chat
            // data and `active_session_key` keeps pointing at the
            // released worker, so the chat view renders the dead
            // worker's history instead of the lead's.
            if matches!(action, forge_workspace::protocol::WorkerStatusAction::Removed) {
                let toast = crate::ui::worker_status::format_close_toast(&status.label, worktree);
                let lead_key = status.spawned_by.clone();
                super::push_system_message_to_session(
                    app,
                    &lead_key,
                    Some(crate::app::SystemSeverity::Info),
                    &toast,
                );
                let worker_key = status.slot.clone();
                let was_active = app.active_session_key.as_ref() == Some(&worker_key);
                let drawn = if was_active { super::drawn_session_order(app) } else { Vec::new() };
                app.sessions.remove(&worker_key);
                if was_active {
                    let fallback = if app.sessions.contains_key(&lead_key) {
                        Some(lead_key)
                    } else {
                        super::adjacent_drawn_session(app, &drawn, &worker_key)
                            .or_else(|| app.sessions.keys().next().cloned())
                    };
                    if let Some(new_active) = fallback {
                        app.switch_active_session(new_active);
                    } else {
                        app.active_session_key = None;
                    }
                }
            }
            app.needs_redraw = true;
        }
        SessionUpdate::PeerEnvelopeAppended { key, wrapped } => {
            // Workspace no longer forges an SDK `Message::User`
            // carrying peer prose - it emits the typed envelope
            // here and the TUI builds the synthetic chat-side
            // user-turn from real fields. The prose is the same
            // string the recipient's LLM sees via Command::Prompt,
            // so the existing `forge_sessions::envelope::detect_inbound` matcher
            // in the SDK-message reducer still recognises it.
            let synthetic = forge_primitives::Message::User {
                message: forge_primitives::UserEnvelope {
                    role: "user".to_owned(),
                    content: vec![forge_primitives::ContentBlock::Text {
                        text: {
                            let prose = wrapped.to_prose();
                            assert_envelope_parses(&prose, "peer_envelope");
                            prose
                        },
                    }],
                },
                session_id: bucket_session_id(app, &key),
                parent_tool_use_id: None,
                uuid: None,
                tool_use_result: None,
            };
            apply_session_update_chat_appended(app, &key, synthetic);
        }
        SessionUpdate::GotifyNotificationAppended { key, notification } => {
            // Mirror the peer-envelope path: forge a synthetic user turn
            // from the notification's prose (the same text the session's
            // LLM sees via Command::Prompt), so `forge_sessions::envelope::detect_inbound`
            // recognises the `[Gotify ...]` prefix and renders the distinct
            // notification block.
            let synthetic = forge_primitives::Message::User {
                message: forge_primitives::UserEnvelope {
                    role: "user".to_owned(),
                    content: vec![forge_primitives::ContentBlock::Text {
                        text: {
                            let prose = notification.to_prose();
                            assert_envelope_parses(&prose, "gotify_notification");
                            prose
                        },
                    }],
                },
                session_id: bucket_session_id(app, &key),
                parent_tool_use_id: None,
                uuid: None,
                tool_use_result: None,
            };
            apply_session_update_chat_appended(app, &key, synthetic);
        }
        SessionUpdate::SlackMessageAppended { key, prose } => {
            // Mirror the gotify path: the workspace hands over the same prose
            // the session's LLM receives, so `forge_sessions::envelope::detect_inbound`
            // recognises the `[Slack ...]` header and renders the block.
            let synthetic = forge_primitives::Message::User {
                message: forge_primitives::UserEnvelope {
                    role: "user".to_owned(),
                    content: vec![forge_primitives::ContentBlock::Text {
                        text: {
                            assert_envelope_parses(&prose, "slack_message");
                            prose
                        },
                    }],
                },
                session_id: bucket_session_id(app, &key),
                parent_tool_use_id: None,
                uuid: None,
                tool_use_result: None,
            };
            apply_session_update_chat_appended(app, &key, synthetic);
        }
        SessionUpdate::CronPromptAppended { key, text } => {
            // Mirror the gotify path: forge a synthetic user turn wrapping
            // the fired prompt in a display-only `[Cron]` prefix so
            // `forge_sessions::envelope::detect_inbound` recognises it and renders the
            // distinct cron block (+ inherits the #383 delivered-turn
            // spinner). The subprocess receives the raw prompt via a
            // separate Command::Prompt, so the bracket never reaches the LLM.
            let synthetic = forge_primitives::Message::User {
                message: forge_primitives::UserEnvelope {
                    role: "user".to_owned(),
                    content: vec![forge_primitives::ContentBlock::Text {
                        text: {
                            let prose = format!("[Cron]\n\n{text}");
                            assert_envelope_parses(&prose, "cron_prompt");
                            prose
                        },
                    }],
                },
                session_id: bucket_session_id(app, &key),
                parent_tool_use_id: None,
                uuid: None,
                tool_use_result: None,
            };
            apply_session_update_chat_appended(app, &key, synthetic);
        }
        // The event's existence is the availability signal; nothing
        // caches it.
        SessionUpdate::DictateAvailability => {}
        SessionUpdate::DictateStarted { key, floor_db, generation } => {
            app.dictate_take_pending = false;
            if let Some(bucket) = app.session_mut(&key) {
                bucket.dictate =
                    Some(crate::app::dictate::DictateIndicator::recording(floor_db, generation));
                bucket.dictate_notice = None;
                let previous = bucket
                    .dictate_border
                    .as_ref()
                    .map(|border| border.current_colour(Instant::now()));
                bucket.dictate_border =
                    Some(crate::app::dictate::DictateBorder::live(previous, Instant::now()));
            }
        }
        SessionUpdate::DictateLevel { key, peak_db } => {
            if let Some(bucket) = app.session_mut(&key)
                && let Some(indicator) = bucket.dictate.as_mut()
            {
                indicator.push_level(peak_db);
            }
        }
        SessionUpdate::DictateTranscribing { key } => {
            if let Some(bucket) = app.session_mut(&key)
                && let Some(indicator) = bucket.dictate.as_mut()
            {
                indicator.begin_transcribing();
            }
        }
        SessionUpdate::DictateProgress { key, generation, done, total } => {
            if let Some(bucket) = app.session_mut(&key)
                && let Some(indicator) = bucket.dictate.as_mut()
                && indicator.generation == generation
            {
                indicator.set_progress(done, total);
            }
        }
        SessionUpdate::DictateEnded { key, outcome, generation } => {
            app.dictate_take_pending = false;
            if let Some(text) = clipboard_text_for_outcome(&outcome) {
                let _ = crate::app::keys::write_text_to_clipboard(text.to_owned());
                let truncated = matches!(
                    outcome,
                    forge_workspace::DictateOutcome::Landed { truncated: true, .. }
                );
                let landing = dictate_landing(app, &key);
                match dictate_destination(app, &key) {
                    Some(editor) => {
                        if landing == DictateLanding::PluginsField {
                            // The plugins targets are single-line fields;
                            // dictated newlines flatten like a paste.
                            editor.insert_str(
                                &crate::app::extensions::normalize_single_line_input(text),
                            );
                            // The overlay field has no selection list to reset.
                            if app.config.add_marketplace_overlay().is_none() {
                                crate::app::extensions::reset_selection_for_active_tab(app);
                            }
                        } else {
                            editor.insert_str(text);
                        }
                    }
                    None => {
                        if let Some(bucket) = app.session_mut(&key) {
                            bucket.input.insert_str(text);
                            // The words arrived in a draft the user was
                            // not looking at; say so until the next
                            // keystroke. An outcome notice (the
                            // truncation warning) stamps over this.
                            bucket.set_dictate_notice(crate::app::dictate::DictateNotice {
                                severity: crate::app::dictate::NoticeSeverity::Dim,
                                text: "dictated words landed here".to_owned(),
                            });
                        } else {
                            tracing::warn!(
                                target: crate::logging::targets::APP_INPUT,
                                event_name = "dictate_transcript_dropped",
                                message = "dictated transcript discarded: session closed mid-take",
                                outcome = "failure",
                            );
                        }
                    }
                }
                // A truncated take warns where its words landed, not
                // only on the chat composer's notice row.
                if truncated {
                    stamp_truncation_warning(app, landing);
                }
            }
            if let Some(bucket) = app.session_mut(&key) {
                apply_dictate_outcome(bucket, &outcome, generation);
            }
        }
    }
    if redraw {
        app.needs_redraw = true;
    }
}

/// The text a resolved take leaves on the clipboard alongside the
/// composer insert: what was inserted, for a landed take; nothing for
/// every other outcome.
fn clipboard_text_for_outcome(outcome: &forge_workspace::DictateOutcome) -> Option<&str> {
    match outcome {
        forge_workspace::DictateOutcome::Landed { text, truncated: _ } => Some(text),
        _ => None,
    }
}

/// Where a landed take's words went - the same question
/// [`dictate_destination`] answers, without the borrow, so the
/// truncation warning can follow the words after the insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DictateLanding {
    Chat,
    Diff,
    PluginsField,
    Fallback,
}

fn dictate_landing(app: &App, key: &forge_workspace::SessionSlot) -> DictateLanding {
    if app.active_session_key.as_ref() != Some(key) {
        return DictateLanding::Fallback;
    }
    match app.input_focus() {
        crate::app::InputFocus::Chat => DictateLanding::Chat,
        crate::app::InputFocus::DiffComment | crate::app::InputFocus::DiffFinishReview => {
            DictateLanding::Diff
        }
        crate::app::InputFocus::None => match app.active_view {
            crate::app::ActiveView::Extensions => DictateLanding::PluginsField,
            _ => DictateLanding::Fallback,
        },
    }
}

/// Stamp the truncation warning on the surface that received the
/// words: the diff overlay's notice line, the plugins status line, or
/// the chat composer's notice row (which `apply_dictate_outcome`
/// stamps from the outcome itself).
fn stamp_truncation_warning(app: &mut App, landing: DictateLanding) {
    let text = crate::app::dictate::truncated_notice_text().to_owned();
    match landing {
        DictateLanding::Diff => {
            if let Some(overlay) = app.diff_overlay.as_mut() {
                overlay.dictate_notice = Some(text);
            }
        }
        DictateLanding::PluginsField => {
            // The plugins view's own status field is rendered by
            // nothing; the config scaffold's status line is what shows.
            app.config.status_message = Some(format!("dictation truncated - {text}"));
        }
        DictateLanding::Chat | DictateLanding::Fallback => {}
    }
}

/// The editor a landed take's words go to: the focused editor of the
/// take's own session - the chat draft, an open diff comment editor,
/// or the finish-review overview - falling back to the owning
/// session's chat draft when another tab holds the focus or no editor
/// is open. The take stays bound to its session whatever the focus.
fn dictate_destination<'a>(
    app: &'a mut App,
    key: &forge_workspace::SessionSlot,
) -> Option<&'a mut crate::app::input::InputState> {
    if app.active_session_key.as_ref() != Some(key) {
        return None;
    }
    match app.input_focus() {
        crate::app::InputFocus::Chat => app.input_mut(),
        crate::app::InputFocus::DiffComment => {
            app.diff_overlay.as_mut()?.active_input.as_mut().map(|input| &mut input.editor)
        }
        crate::app::InputFocus::DiffFinishReview => {
            app.diff_overlay.as_mut()?.finish_review.as_mut().map(|finish| &mut finish.editor)
        }
        crate::app::InputFocus::None => match app.active_view {
            crate::app::ActiveView::Extensions => {
                // The add-marketplace field when its overlay is up, else
                // the focused tab's search query.
                if let Some(overlay) = app.config.add_marketplace_overlay_mut() {
                    return Some(&mut overlay.editor);
                }
                if app.plugins.search_focused {
                    return app.plugins.active_search_query_mut();
                }
                None
            }
            _ => None,
        },
    }
}

/// `SessionUpdate::DictateEnded` reducer. The landed text was routed
/// before this runs; what is left is the notice, stamped against the
/// draft version the insert produced so the next keystroke clears it,
/// and the indicator reset. Only the take's own generation resets the
/// indicator: a stale resolver arriving after a newer take started on
/// the same key says its piece and leaves the live take alone.
fn apply_dictate_outcome(
    bucket: &mut crate::app::session::UiSession,
    outcome: &forge_workspace::DictateOutcome,
    generation: u64,
) {
    let floor_db = bucket.dictate.as_ref().map_or(-50.0, |indicator| indicator.floor_db);
    let notice = crate::app::dictate::notice_for_outcome(outcome, floor_db);
    let live_generation = bucket.dictate.as_ref().map(|indicator| indicator.generation);
    if live_generation == Some(generation) {
        let beat = matches!(outcome, forge_workspace::DictateOutcome::Landed { .. });
        let frozen = bucket.dictate_border.take().map(|border| border.rgb());
        bucket.dictate = None;
        bucket.dictate_border = frozen.map(|rgb| crate::app::dictate::DictateBorder::Afterglow {
            started: Instant::now(),
            rgb,
            beat,
        });
    }
    if let Some(notice) = notice {
        bucket.set_dictate_notice(notice);
    }
}

/// `SessionUpdate::Spawning` reducer. Synthesize a placeholder
/// bucket under `key` with a "Waking {display_name}…" message, set
/// `cwd_raw`/`cwd` from the project's path, and (conditionally)
/// switch active focus.
///
/// **Focus rule:** a spawn moves focus only when a person asked for
/// that session, or when it belongs to the project the CLI named and
/// nothing is focused. A click records its key in
/// [`App::pending_spawn_focus`], so the reducer completes that move
/// when the bucket appears; an `auto_start` spawn arrives unasked and
/// registers in the background, which is what keeps a launchpad boot
/// on its picker.
///
/// Both branches share one gate: [`App::arriving_session_takes_the_tab`].
/// An existing bucket whose wake nobody asked for - a cron, peer,
/// gotify or slack repeat hitting the stub an earlier failed spawn left
/// behind - registers silently. A click on a stub row never reaches
/// this reducer at all: the click handler switches or refuses directly.
fn apply_session_update_spawning(
    app: &mut App,
    key: SessionSlot,
    project_name: &str,
    cwd: &str,
    display_name: &str,
) {
    if app.sessions.contains_key(&key) {
        // Same gate as the fresh-bucket path below: only a click that
        // asked for THIS wake moves focus. A background SpawnProject
        // (cron, peer prompt, gotify or slack delivery) hitting a stale
        // stub left by an earlier failed spawn must not yank the tab
        // away from whatever holds it.
        let user_asked_for_this = app.pending_spawn_focus.as_deref() == Some(project_name);
        if user_asked_for_this {
            app.pending_spawn_focus = None;
        }
        if user_asked_for_this || app.arriving_session_takes_the_tab(project_name) {
            tracing::info!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "spawn_wake_focus",
                outcome = "focused",
                reason = if user_asked_for_this { "user_asked" } else { "boot_project" },
                slot = %key.display(),
            );
            app.switch_active_session(key);
        } else {
            tracing::info!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "spawn_wake_focus",
                outcome = "registered",
                reason = "background_wake",
                slot = %key.display(),
            );
            app.needs_redraw = true;
        }
        return;
    }
    // Stamp the tab's forge.toml project NAME up front so the Inspector
    // scopes SCHEDULES / GOTIFY correctly even during the Waking phase.
    // The Spawning payload names the project directly for a project /
    // session spawn, so the catalog lookup canonicalises it and a cwd
    // resolve catches a stray key or display-path form. The payload's
    // own name is the last resort: it is the name the workspace
    // spawned the session under, not a guess.
    let project = app
        .workspace
        .as_ref()
        .and_then(|ws| {
            ws.list_projects()
                .into_iter()
                .find(|view| view.name == project_name)
                .map(|view| view.name)
                .or_else(|| ws.project_name_for_path(cwd))
        })
        .unwrap_or_else(|| project_name.to_owned());
    let mut bucket = crate::app::session::UiSession::new(key.clone(), project);
    bucket.cwd = shorten_cwd_display_path(cwd);
    cwd.clone_into(&mut bucket.cwd_raw);
    bucket.messages.push(crate::app::ChatMessage::new(
        crate::app::MessageRole::System(Some(crate::app::SystemSeverity::Info)),
        vec![crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(&format!(
            "Waking {display_name}…"
        )))],
    ));
    bucket.message_retained_bytes.push(0);
    app.sessions.insert(key.clone(), bucket);
    super::set_bucket_lifecycle_state(
        app,
        &key,
        crate::app::session::SessionLifecycleState::Spawning,
    );

    // **Focus stays where it is** for a wake nobody asked for.
    // Auto-focusing a background wake would let whichever auto_start
    // project's Spawning event arrives first steal the screen, so such
    // a wake only registers its bucket and triggers a redraw.
    //
    // A user-driven wake is the exception: the click recorded this
    // project in `pending_spawn_focus`, so honouring it moves focus for
    // that one spawn and no other. The project named on the command
    // line does not reach this reducer at all - its bucket is minted by
    // the `Connected` that follows.
    let user_asked_for_this = app.pending_spawn_focus.as_deref() == Some(project_name);
    if user_asked_for_this {
        app.pending_spawn_focus = None;
    }
    if user_asked_for_this || app.arriving_session_takes_the_tab(project_name) {
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "spawn_wake_focus",
            outcome = "focused",
            reason = if user_asked_for_this { "user_asked" } else { "boot_project" },
            slot = %key.display(),
        );
        app.switch_active_session(key);
    } else {
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "spawn_wake_focus",
            outcome = "registered",
            reason = "background_wake",
            slot = %key.display(),
        );
        app.needs_redraw = true;
    }
}

fn shorten_cwd_display_path(cwd: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy().to_string();
        if cwd.starts_with(&home_str) {
            return format!("~{}", &cwd[home_str.len()..]);
        }
    }
    cwd.to_owned()
}

/// `SessionUpdate::ForgeAccountIdentity` reducer for the
/// session bucket addressed by `key`. Active-session targeting goes
/// through the existing
/// [`crate::app::App::set_active_account_display_name`] accessor +
/// [`crate::app::App::sync_welcome_snapshot`] so welcome rendering
/// updates promptly. Background-session targeting writes the
/// display name directly into the bucket without touching the
/// active-session welcome snapshot.
pub(super) fn apply_session_update_forge_account_identity(
    app: &mut App,
    key: &SessionSlot,
    display_name: String,
) {
    apply_forge_account_identity_presentation(app, key, display_name);
}

fn apply_forge_account_identity_presentation(
    app: &mut App,
    session_key: &SessionSlot,
    display_name: String,
) {
    if app.active_session_key.as_ref() == Some(session_key) {
        app.set_active_account_display_name(Some(display_name));
        app.sync_welcome_snapshot();
        return;
    }
    // Background-session path: write directly to the bucket.
    if let Some(bucket) = app.sessions.get_mut(session_key) {
        bucket.active_account_display_name = Some(display_name);
    } else {
        // A known defect, not noise: every occurrence measured is a
        // worker session key, so the identity is lost for every worker.
        // Do not demote this line to make the log quiet.
        tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "forge_account_identity_dropped",
            message = "forge-account identity dropped for an unknown session",
            outcome = "dropped",
            slot = %session_key.display(),
            reason = "unknown_session",
        );
    }
}

/// `SessionUpdate::StatusSnapshot` reducer for the
/// session bucket addressed by `session_id`. Routes through the
/// active-session accessors when targeting the rendered session
/// (so welcome + Status panel rerender promptly); writes directly
/// into the bucket otherwise so background sessions accumulate
/// state silently.
pub(super) fn apply_session_update_status_snapshot(
    app: &mut App,
    key: &SessionSlot,
    account: forge_primitives::AccountInfo,
    forge_account: Option<forge_primitives::ForgeAccountIdentity>,
) {
    apply_status_snapshot_presentation(app, key, account, forge_account);
}

fn apply_status_snapshot_presentation(
    app: &mut App,
    key: &SessionSlot,
    account: forge_primitives::AccountInfo,
    forge_account: Option<forge_primitives::ForgeAccountIdentity>,
) {
    let session_key = key.clone();
    let has_email = account.email.as_deref().is_some_and(|email| !email.trim().is_empty());
    let has_organization = account.organization.is_some();
    let subscription_type = account.subscription_type.clone();
    let token_source = account.token_source.clone();
    let api_key_source = account.api_key_source.clone();
    let api_provider = account.api_provider.clone();
    let forge_display_name = forge_account.as_ref().map(|f| f.display_name.clone());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        app.set_account_info(Some(account));
        app.set_active_account_display_name(forge_account.map(|f| f.display_name));
        app.sync_welcome_snapshot();
    } else if let Some(bucket) = app.sessions.get_mut(&session_key) {
        // Background-session path: write directly to the bucket.
        bucket.account_info = Some(account);
        bucket.active_account_display_name = forge_account.map(|f| f.display_name);
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "status_snapshot_dropped",
            message = "status snapshot dropped for an unknown session",
            outcome = "dropped",
            slot = %key.display(),
            reason = "unknown_session",
        );
        return;
    }
    tracing::info!(
        target: crate::logging::targets::APP_AUTH,
        event_name = "status_snapshot_applied",
        message = "status snapshot applied",
        outcome = "success",
        slot = %key.display(),
        is_active,
        has_email,
        has_organization,
        subscription_type = ?subscription_type,
        token_source = ?token_source,
        api_key_source = ?api_key_source,
        api_provider = ?api_provider,
        forge_display_name = ?forge_display_name,
    );
}

/// `SessionUpdate::OauthCredentialsSnapshot` reducer for
/// the session bucket addressed by `session_id`. Active-session
/// targeting goes through [`crate::app::App::set_oauth_credentials`];
/// background-session targeting writes directly into the bucket.
pub(super) fn apply_session_update_oauth_credentials_snapshot(
    app: &mut App,
    key: &SessionSlot,
    credentials: Option<forge_primitives::cloud::oauth_credentials::OauthCredentials>,
) {
    apply_oauth_credentials_snapshot_presentation(app, key, credentials);
}

fn apply_oauth_credentials_snapshot_presentation(
    app: &mut App,
    key: &SessionSlot,
    credentials: Option<forge_primitives::cloud::oauth_credentials::OauthCredentials>,
) {
    let session_key = key.clone();
    let has_credentials = credentials.is_some();
    let has_expiry = credentials.as_ref().is_some_and(|info| info.expires_at.is_some());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        app.set_oauth_credentials(credentials);
    } else if let Some(session) = app.session_mut(&session_key) {
        session.oauth_credentials = credentials;
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "oauth_credentials_snapshot_dropped",
            message = "oauth credentials snapshot dropped for an unknown session",
            outcome = "dropped",
            slot = %key.display(),
            reason = "unknown_session",
        );
        return;
    }
    tracing::info!(
        target: crate::logging::targets::APP_AUTH,
        event_name = "oauth_credentials_snapshot_applied",
        message = "oauth credentials snapshot applied",
        outcome = "success",
        slot = %key.display(),
        is_active,
        has_credentials,
        has_expiry,
    );
}

/// `SessionUpdate::ContextUsageSnapshot` reducer for the
/// session bucket addressed by `session_id`. Active-session
/// targeting goes through
/// [`crate::app::session_runtime::apply_context_usage_snapshot`] so
/// the in-flight refresh chaining still kicks in. Background-session
/// targeting writes directly into the bucket and skips the refresh
/// chain (a background session re-requests on next active switch).
pub(super) fn apply_session_update_context_usage_snapshot(
    app: &mut App,
    key: &SessionSlot,
    percentage: Option<u8>,
    max_tokens: Option<u64>,
) {
    apply_context_usage_snapshot_presentation(app, key, percentage, max_tokens);
}

fn apply_context_usage_snapshot_presentation(
    app: &mut App,
    key: &SessionSlot,
    percentage: Option<u8>,
    max_tokens: Option<u64>,
) {
    let session_key = key.clone();
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        crate::app::session_runtime::apply_context_usage_snapshot(app, percentage, max_tokens);
    } else if let Some(session) = app.session_mut(&session_key) {
        session.session_usage.context_usage_percent = percentage;
        session.session_usage.context_max_tokens = max_tokens;
        session.session_usage.context_usage_in_flight = false;
        // Drop the refresh-pending flag too - once a fresh value
        // landed, queueing another refresh is wasteful for a
        // background bucket.
        session.session_usage.context_usage_refresh_pending = None;
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "context_usage_dropped",
            message = "context usage dropped for an unknown session",
            outcome = "dropped",
            slot = %key.display(),
            reason = "unknown_session",
        );
    }
}

/// `SessionUpdate::McpSnapshot` reducer for the session
/// bucket addressed by `session_id`. Active-session targeting also
/// reconciles the App-global MCP auth-redirect overlay and selection
/// index. Background-session targeting only writes the per-session
/// MCP state into the bucket - the overlay reconciliation is
/// inherently active-session UI.
/// Returns whether anything the active view renders actually changed -
/// the background poll re-asks on a timer, and an identical snapshot
/// must not wake the render loop.
pub(super) fn apply_session_update_mcp_snapshot(
    app: &mut App,
    key: &SessionSlot,
    servers: Vec<forge_primitives::McpServerStatus>,
    error: Option<String>,
) -> bool {
    apply_mcp_snapshot_presentation(app, key, servers, error)
}

fn apply_mcp_snapshot_presentation(
    app: &mut App,
    key: &SessionSlot,
    servers: Vec<forge_primitives::McpServerStatus>,
    error: Option<String>,
) -> bool {
    let session_key = key.clone();
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    let server_count = servers.len();
    let error_present = error.is_some();
    let changed = if is_active {
        if let Some(mcp) = app.mcp_mut() {
            let changed = mcp.servers != servers || mcp.in_flight || error.is_some();
            mcp.servers = servers;
            mcp.in_flight = false;
            // Only a snapshot carrying its own error overwrites the slot.
            // A reconnect failure arrives on `handle_mcp_operation_error`,
            // so a background poll's `error: None` would erase it before
            // the user has seen it. User-initiated refreshes still clear
            // it - `refresh_mcp_snapshot` nulls it at request time.
            if error.is_some() {
                mcp.last_error = error;
            }
            changed
        } else {
            false
        }
    } else if let Some(session) = app.session_mut(&session_key) {
        session.mcp.servers = servers;
        session.mcp.in_flight = false;
        if error.is_some() {
            session.mcp.last_error = error;
        }
        // A background bucket's rows aren't on screen; the caller's
        // active-session gate already suppresses the repaint.
        false
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "mcp_snapshot_dropped",
            message = "MCP snapshot dropped for an unknown session",
            outcome = "dropped",
            slot = %key.display(),
            reason = "unknown_session",
        );
        return false;
    };
    if changed && is_active {
        // The Mcps tab's selection lives in the shared per-tab state;
        // a snapshot that shrank the server list must not strand it.
        crate::app::extensions::clamp_mcps_selection(app);
    }
    tracing::info!(
        target: crate::logging::targets::APP_CONFIG,
        event_name = "mcp_snapshot_applied",
        message = "MCP snapshot applied",
        outcome = "success",
        slot = %key.display(),
        is_active,
        server_count,
        error_present,
    );
    changed
}

/// `SessionUpdate::ChatAppended` reducer for the session bucket
/// addressed by `session_id`. The SDK message dispatcher uses
/// active-session UI accessors (chat buffer, tool-call indices,
/// viewport); [`apply_sdk_message_presentation`] temp-swaps
/// `active_session_key` to route background sessions through the
/// same path.
/// The four envelope reducers - peer, gotify, cron and slack - forge a
/// synthetic `Message::User` whose only job is to be re-parsed by
/// `detect_inbound`. If that parse fails
/// the chat echo is dropped silently while the LLM still receives the
/// prose via `Command::Prompt` - the agent works on a message the user
/// never saw arrive. forge-workspace cannot depend on forge-tui, so no
/// test spans the round trip; this is the assertion that catches a
/// prose-format drift at runtime.
fn assert_envelope_parses(prose: &str, source: &'static str) {
    if forge_sessions::envelope::detect_inbound(prose).is_none() {
        let head: String = prose.chars().take(120).collect();
        tracing::error!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "envelope_prose_unrecognised",
            source,
            outcome = "chat_echo_dropped",
            prose_head = %head,
            "forged envelope prose did not match detect_inbound; the LLM still \
             received it but the user will not see it",
        );
    }
}

pub(super) fn apply_session_update_chat_appended(
    app: &mut App,
    key: &SessionSlot,
    msg: forge_primitives::Message,
) {
    apply_sdk_message_presentation(app, key, msg);
}

fn apply_sdk_message_presentation(
    app: &mut App,
    key: &SessionSlot,
    msg: forge_primitives::Message,
) {
    // Every frame arrives addressed to the slot its producer stated, so
    // attribution never depends on the wire `session_id`: no bucket is
    // inferred from an id, and a frame for a session this process does
    // not hold is dropped rather than adopted by whichever bucket is
    // focused.
    //
    // The id is still adopted onto the addressed bucket when it has none
    // yet: for new sessions the CLI doesn't emit `system/init` until
    // AFTER the first user message lands (per `Client::spawn` docs), so
    // `Client::session_id()` is empty at spawn time. For resume the
    // bridge already used the resume_id for Connected, so adoption is a
    // no-op.
    let active_session_id_string = app.session_id().map(|s| s.to_string());
    let active_session_id_str = active_session_id_string.as_deref().unwrap_or("");
    let is_active = app.active_session_key.as_ref() == Some(key);
    if !app.sessions.contains_key(key) {
        // No bucket holds this slot: a frame for a session this process
        // does not have, not a key-drift race. If the dropped msg is
        // `Result`, TurnComplete never fires and the turn info row spins
        // and counts up forever.
        let bucket_keys: Vec<String> =
            app.sessions.keys().map(forge_workspace::SessionSlot::display).collect();
        tracing::error!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "sdk_message_dropped",
            message = "SDK message dropped for an unknown session",
            outcome = "dropped",
            slot = %key.display(),
            active_session_id = %active_session_id_str,
            msg_variant = msg_variant_name(&msg),
            bucket_keys = ?bucket_keys,
            reason = "unknown_session",
        );
        return;
    }
    if is_active {
        if active_session_id_str.is_empty()
            && let Some(wire_id) = msg.session_id()
            && !wire_id.is_empty()
        {
            tracing::info!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "sdk_frame_id_adopted",
                outcome = "success",
                slot = %key.display(),
                adopted_session_id = %wire_id,
            );
            app.set_session_id(Some(crate::agent::model::SessionId::new(wire_id.to_owned())));
        }
        super::sdk_message::handle_sdk_message(app, msg);
        return;
    }
    // SDK message for a non-active session. The handlers in
    // `super::sdk_message::handle_sdk_message` reach for the
    // active bucket via the App-level accessors (chat buffer,
    // tool-call indices, viewport, …). Temporarily promote the
    // target bucket to active so those accessors land on the
    // right session, then dispatch and restore. The active
    // session's `App.input` and `App.status` are snapshotted +
    // restored across the swap so background routing doesn't
    // touch user-visible UI for the session the user is actually
    // looking at. Without this routing, background turns produce
    // events that update lifecycle state (via routed handlers in
    // `events/turn.rs`) but never land their `Message::Assistant`
    // payloads in the bucket - the user switches back to a
    // bucket whose pane glyph says Attention but whose chat
    // buffer still only shows what was on screen at switch-out.
    let success_result = matches!(
        &msg,
        forge_primitives::Message::Result { is_error, subtype, .. }
            if super::sdk_message::is_success_result(*is_error, subtype)
    );
    crate::app::active_bucket_scope::with_pivoted(app, key.clone(), |app| {
        super::sdk_message::handle_sdk_message(app, msg);
    });
    // Under the pivot the finalize path sees the background key as
    // active and never reaches the reducer's background arm, so
    // the dispatcher is the writer production reaches: a success
    // Result on a non-active bucket arms its unseen-completion
    // flag and raises the completion ping.
    if success_result && let Some(bucket) = app.sessions.get_mut(key) {
        bucket.unseen_turn_completion = true;
        app.notify(crate::app::notify::NotifyEvent::TurnComplete, key);
    }
    app.needs_redraw = true;
}

/// `SessionUpdate::HookObservation` reducer for the
/// session bucket addressed by `session_id`. Active-session
/// targeting goes through the App accessors so the mode/effort
/// chips update promptly. Background-session targeting writes
/// directly into the bucket so its observed values stay current
/// for a future switch. Wraps the `String` fields from the
/// workspace payload into `&str` borrows before delegating to the
/// shared presentation helper.
pub(super) fn apply_session_update_hook_observation(
    app: &mut App,
    key: &SessionSlot,
    tool_use_id: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    agent_id: Option<&str>,
    agent_type: Option<&str>,
) {
    apply_hook_observation_presentation(
        app,
        key,
        tool_use_id,
        permission_mode,
        effort,
        agent_id,
        agent_type,
    );
}

fn apply_hook_observation_presentation(
    app: &mut App,
    key: &SessionSlot,
    tool_use_id: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    agent_id: Option<&str>,
    agent_type: Option<&str>,
) {
    use crate::agent::model::EffortLevel;
    use forge_workspace::PermissionMode;

    let session_key = key.clone();
    let is_active = app.active_session_key.as_ref() == Some(&session_key);

    let parsed_permission_mode = permission_mode.and_then(PermissionMode::from_wire);
    let parsed_effort = effort.and_then(|effort_str| match effort_str {
        "low" => Some(EffortLevel::Low),
        "medium" => Some(EffortLevel::Medium),
        "high" => Some(EffortLevel::High),
        "xhigh" => Some(EffortLevel::Xhigh),
        "max" => Some(EffortLevel::Max),
        _ => {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                effort = %effort_str,
                "hook_observation: unknown effort level; ignored",
            );
            None
        }
    });

    if is_active {
        if let Some(mode) = parsed_permission_mode {
            app.set_observed_permission_mode(Some(mode));
        }
        if let Some(level) = parsed_effort {
            app.set_observed_effort(Some(level));
        }
        if let (Some(tool_use_id), Some(_agent_id), Some(agent_type)) =
            (tool_use_id, agent_id, agent_type)
            && let Some(attribution) = app.subagent_attribution_mut()
        {
            attribution.insert(tool_use_id.to_owned(), agent_type.to_owned());
        }
    } else if let Some(session) = app.session_mut(&session_key) {
        if let Some(mode) = parsed_permission_mode {
            session.observed_permission_mode = Some(mode);
        }
        if let Some(level) = parsed_effort {
            session.observed_effort = Some(level);
        }
        if let (Some(tool_use_id), Some(_agent_id), Some(agent_type)) =
            (tool_use_id, agent_id, agent_type)
        {
            session.subagent_attribution.insert(tool_use_id.to_owned(), agent_type.to_owned());
        }
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "hook_observation_dropped",
            message = "hook observation dropped for an unknown session",
            outcome = "dropped",
            slot = %key.display(),
            reason = "unknown_session",
        );
    }
}

/// `SessionUpdate::RuntimeReloadCompleted` reducer for the
/// session bucket addressed by `session_id`. The plugins config tab
/// is App-global UI scoped to the active session - a background
/// reload that completes silently is a no-op on the UI but logged
/// so the operator can confirm the bridge dispatched it. Unknown-
/// session events log a warn-level breadcrumb.
pub(super) fn apply_session_update_runtime_reload_completed(app: &mut App, key: &SessionSlot) {
    apply_runtime_reload_completed_presentation(app, key);
}

fn apply_runtime_reload_completed_presentation(app: &mut App, key: &SessionSlot) {
    let session_key = key.clone();
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        crate::app::extensions::apply_runtime_reload_success(app);
    } else if app.sessions.contains_key(&session_key) {
        // A background reload cannot apply, but the pane's loading flag
        // is App-global: if the reload armed it (a guarded entry point
        // started while this session was focused), leaving it set
        // wedges every later guarded action until a session reset.
        crate::app::extensions::settle_dropped_refresh_failure(app);
        tracing::debug!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "runtime_reload_completed_background",
            message = "runtime reload completed for a background session; UI unaffected",
            outcome = "info",
            slot = %key.display(),
        );
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "runtime_reload_completed_dropped",
            message = "runtime reload completion dropped for an unknown session",
            outcome = "dropped",
            slot = %key.display(),
            reason = "unknown_session",
        );
    }
}

/// `SessionUpdate::RuntimeReloadFailed` reducer for the
/// session bucket addressed by `session_id`. Same routing shape as
/// [`apply_session_update_runtime_reload_completed`].
pub(super) fn apply_session_update_runtime_reload_failed(
    app: &mut App,
    key: &SessionSlot,
    message: &str,
) {
    apply_runtime_reload_failed_presentation(app, key, message);
}

fn apply_runtime_reload_failed_presentation(app: &mut App, key: &SessionSlot, message: &str) {
    let session_key = key.clone();
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        crate::app::extensions::apply_runtime_reload_failure(app, message);
    } else if app.sessions.contains_key(&session_key) {
        // Same wedge as the completed arm: an App-global armed flag
        // must not survive a background reload it can never see.
        crate::app::extensions::settle_dropped_refresh_failure(app);
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "runtime_reload_failed_background",
            message = "runtime reload failed for a background session; UI unaffected",
            outcome = "degraded",
            slot = %key.display(),
            error_message = %message,
        );
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "runtime_reload_failed_dropped",
            message = "runtime reload failure dropped for an unknown session",
            outcome = "dropped",
            slot = %key.display(),
            reason = "unknown_session",
        );
    }
}

#[cfg(test)]
mod tests {
    use forge_workspace::protocol::WorktreeDisposition;

    use super::*;
    use crate::app::session::UiSession;

    /// Only a landed take rides the clipboard along, and it copies
    /// exactly what was inserted - truncated or whole; every other
    /// outcome copies nothing.
    #[test]
    fn clipboard_text_follows_the_landed_outcome_only() {
        assert_eq!(
            clipboard_text_for_outcome(&forge_workspace::DictateOutcome::Landed {
                text: "run just check".to_owned(),
                truncated: false,
            }),
            Some("run just check"),
            "a landed take copies its words"
        );
        assert_eq!(
            clipboard_text_for_outcome(&forge_workspace::DictateOutcome::Landed {
                text: "this is what fitted".to_owned(),
                truncated: true,
            }),
            Some("this is what fitted"),
            "a truncated take copies what was inserted, not what was said"
        );
        assert_eq!(
            clipboard_text_for_outcome(&forge_workspace::DictateOutcome::Empty),
            None,
            "an empty take copies nothing"
        );
        assert_eq!(
            clipboard_text_for_outcome(&forge_workspace::DictateOutcome::NoAudio {
                peak_db: -38.2,
                seconds: 4,
            }),
            None,
            "a quiet take copies nothing"
        );
        assert_eq!(
            clipboard_text_for_outcome(&forge_workspace::DictateOutcome::Cancelled),
            None,
            "a cancelled take copies nothing"
        );
        assert_eq!(
            clipboard_text_for_outcome(&forge_workspace::DictateOutcome::Failed),
            None,
            "a failed take copies nothing"
        );
    }

    /// The catalog-loaded event carries no session key, so it rides
    /// the global-event path and wakes the render loop - the frame the
    /// launchpad's session counts appear on.
    #[test]
    fn catalog_loaded_flips_needs_redraw() {
        let mut app = App::test_default();
        app.needs_redraw = false;

        apply_session_update(&mut app, forge_workspace::SessionUpdate::CatalogLoaded);

        assert!(app.needs_redraw, "the scan landing must trigger the frame that shows its counts");
    }

    /// The echo is the only source the dialog's markers and reset row
    /// read, so it must land on the addressed bucket and nothing else.
    #[test]
    fn dictate_override_echo_lands_on_the_addressed_bucket_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);

        let overrides = forge_workspace::DictateOverrides {
            styling: Some(forge_workspace::Styling::Formal),
            context: Some(forge_workspace::Context::Email),
            ..Default::default()
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::DictateOverrides { key: key_a.clone(), overrides },
        );

        assert_eq!(app.sessions[&key_a].dictate_overrides, overrides);
        assert_eq!(
            app.sessions[&key_b].dictate_overrides,
            forge_workspace::DictateOverrides::default(),
            "an echo for one session must not touch another"
        );
        assert!(app.needs_redraw);
    }

    /// The device-pick echo carries the dispatching session's key, but
    /// the pick is APP state: one echo lands no matter which session it
    /// names, and every session's readout reads the same field.
    #[test]
    fn the_device_pick_echo_lands_on_the_shared_state() {
        let mut app = App::test_default();
        app.needs_redraw = false;
        let (key_a, _key_b) = seed_two_sessions(&mut app);

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::DictateDevicePin {
                key: key_a,
                pick: Some(forge_workspace::DictateDeviceChoice::System),
            },
        );

        assert_eq!(
            app.dictate_device_pin,
            Some(forge_workspace::DictateDeviceChoice::System),
            "the pick is shared by every session, so the echo must land on App state"
        );
        assert!(app.needs_redraw, "every session's readout renders from this field");
    }

    fn seed_two_sessions(app: &mut App) -> (SessionSlot, SessionSlot) {
        let key_a = SessionSlot::from_str_for_test("session-a");
        let key_b = SessionSlot::from_str_for_test("session-b");
        let mut bucket_a = UiSession::new(key_a.clone(), "test-project");
        bucket_a.session_id = Some(forge_primitives::SessionId::new(key_a.display()));
        let mut bucket_b = UiSession::new(key_b.clone(), "test-project");
        bucket_b.session_id = Some(forge_primitives::SessionId::new(key_b.display()));
        app.sessions.insert(key_a.clone(), bucket_a);
        app.sessions.insert(key_b.clone(), bucket_b);
        // Register a DomainSession for each so AgentHandle dispatch
        // (which still needs an internal session_id mirror on the
        // workspace side) can route through.
        if let Some(ws) = app.workspace.as_ref() {
            for k in [&key_a, &key_b] {
                let (h, _) = forge_workspace::Workspace::testing_stub_handle();
                let dom = ws.register_domain_session(k.clone(), Some(std::sync::Arc::new(h)));
                dom.lock().session_id = Some(forge_primitives::SessionId::new(k.display()));
            }
        }
        app.active_session_key = Some(key_a.clone());
        app.needs_redraw = false;
        (key_a, key_b)
    }

    /// A `Message::Result` frame: `is_error: false` is the success
    /// shape a completed turn arrives as, `is_error: true` the failed
    /// shape that must not arm the unseen-completion flag.
    fn result_frame(session_id: &str, is_error: bool) -> forge_primitives::Message {
        forge_primitives::Message::Result {
            subtype: "success".to_owned(),
            session_id: session_id.to_owned(),
            is_error,
            num_turns: 1,
            duration_ms: 0,
            duration_api_ms: 0,
            stop_reason: Some("end_turn".to_owned()),
            total_cost_usd: None,
            usage: None,
            result: None,
            structured_output: None,
            model_usage: None,
            permission_denials: None,
            errors: None,
            uuid: None,
            terminal_reason: None,
        }
    }

    /// Read the `account_info` field on the bucket for `key`.
    fn bucket_account_info_for(
        app: &App,
        key: &SessionSlot,
    ) -> Option<forge_primitives::AccountInfo> {
        app.sessions.get(key).and_then(|s| s.account_info.clone())
    }

    /// Multiplexer-isolation test: a `StatusSnapshotReceived` event
    /// tagged for session B updates B's bucket without touching
    /// session A's bucket and without flipping `needs_redraw` -
    /// `needs_redraw` flips only for events that target the active
    /// session (A in this fixture). Proves the per-session
    /// multiplexer correctly routes background-session events.
    #[test]
    fn background_event_updates_target_session_only() {
        let mut app = App::test_default();

        // Two real session buckets keyed off claude-issued UUIDs.
        // The seeded test bucket from `App::test_default` stays in
        // the map; we only care that A and B are present.
        let (key_a, key_b) = seed_two_sessions(&mut app);
        assert!(bucket_account_info_for(&app, &key_a).is_none());
        assert!(bucket_account_info_for(&app, &key_b).is_none());

        // Fire a state-change event tagged for B.
        let account = forge_primitives::AccountInfo {
            email: Some("b@example.com".to_owned()),
            ..Default::default()
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                key: key_b.clone(),
                account,
                forge_account: None,
            },
        );

        // B's domain reflects the change.
        let b_info = bucket_account_info_for(&app, &key_b).expect("b account_info");
        assert_eq!(b_info.email.as_deref(), Some("b@example.com"));

        // A's domain is untouched.
        assert!(
            bucket_account_info_for(&app, &key_a).is_none(),
            "session A's account_info must not be set"
        );

        // Active session is A; redraw flag must NOT flip for an event
        // routed to a background bucket.
        assert!(!app.needs_redraw, "needs_redraw must stay false for background-session events");
    }

    fn session_replaced_for(key: &SessionSlot, session_id: &str, cwd: &str) -> SessionUpdate {
        SessionUpdate::SessionReplaced {
            key: key.clone(),
            session_id: forge_primitives::SessionId::new(session_id.to_owned()),
            cwd: cwd.to_owned(),
            current_model: test_current_model(),
            available_models: Vec::new(),
            mode: None,
            history: Vec::new(),
            compaction_count: 0,
        }
    }

    /// A `SessionReplaced` naming background session B must reset B's
    /// own bucket and touch nothing else. The slot keeps its bucket, so
    /// there is no key to migrate onto: B's contents reset in place and
    /// A - the tab the user is looking at - is left alone.
    #[test]
    fn session_replaced_for_background_session_leaves_the_active_tab_alone() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        if let Some(bucket) = app.sessions.get_mut(&key_a) {
            bucket.cwd_raw = "/proj-a".to_owned();
            bucket.cwd = "/proj-a".to_owned();
        }

        apply_session_update(&mut app, session_replaced_for(&key_b, "b-replacement", "/proj-b"));

        assert_eq!(app.active_session_key.as_ref(), Some(&key_a), "focus stays on A");
        assert!(app.sessions.contains_key(&key_a), "A's bucket survives B's replacement");
        assert_eq!(
            app.session_id().map(|id| id.to_string()).as_deref(),
            Some(key_a.display().as_str()),
            "A keeps its own session id",
        );
        assert_eq!(app.cwd_raw().as_deref(), Some("/proj-a"), "A keeps its own cwd");
        let bucket_b = app.sessions.get(&key_b).expect("B keeps its own slot");
        assert_eq!(bucket_b.cwd_raw, "/proj-b");
        assert_eq!(
            bucket_b.session_id.as_ref().map(ToString::to_string).as_deref(),
            Some("b-replacement"),
        );
    }

    /// The foreground arm clears per-bucket cancel + compaction state
    /// through the active-bucket accessors. The background arm has to
    /// reach the same fields directly or they surface on the next switch
    /// to that tab as a stale "press Esc again" hint and a stale
    /// compacting indicator.
    #[test]
    fn session_replaced_for_background_session_clears_its_per_bucket_turn_state() {
        let mut app = App::test_default();
        let (_key_a, key_b) = seed_two_sessions(&mut app);
        if let Some(bucket) = app.sessions.get_mut(&key_b) {
            bucket.pending_cancel = true;
            bucket.is_compacting = true;
            bucket.pending_compact_clear = true;
        }

        apply_session_update(&mut app, session_replaced_for(&key_b, "b-replacement", "/proj-b"));

        let bucket = app.sessions.get(&key_b).expect("B keeps its own slot");
        assert!(!bucket.pending_cancel, "a replaced session has no cancel in flight");
        assert!(!bucket.is_compacting);
        assert!(!bucket.pending_compact_clear);
    }

    /// Foreground twin: replacing the session the user is watching
    /// swaps its occupant inside the same slot, so focus stays put and
    /// only the contents reset.
    #[test]
    fn session_replaced_for_the_active_session_keeps_focus_on_the_slot() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);

        apply_session_update(&mut app, session_replaced_for(&key_a, "a-replacement", "/proj-a"));

        assert_eq!(app.active_session_key.as_ref(), Some(&key_a), "focus stays on the slot");
        assert!(app.sessions.contains_key(&key_a), "A's bucket stays in its place");
        assert!(app.sessions.contains_key(&key_b), "background B is untouched");
        assert_eq!(app.cwd_raw().as_deref(), Some("/proj-a"));
        assert_eq!(
            app.session_id().map(|id| id.to_string()).as_deref(),
            Some("a-replacement"),
            "the slot's contents carry the new occupant",
        );
    }

    #[test]
    fn status_snapshot_routes_to_active_session_and_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        let account = forge_primitives::AccountInfo {
            email: Some("a@example.com".to_owned()),
            ..Default::default()
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                key: key_a.clone(),
                account,
                forge_account: None,
            },
        );
        let a_info = bucket_account_info_for(&app, &key_a).expect("a account_info");
        assert_eq!(a_info.email.as_deref(), Some("a@example.com"));
        assert!(bucket_account_info_for(&app, &key_b).is_none());
        assert!(app.needs_redraw);
    }

    /// The `CronPromptAppended` reducer surfaces a fired cron: a distinct
    /// cron block (a User turn carrying the fired prompt, flagged
    /// `is_cron_envelope`) AND the #383 delivered-turn spinner - a fresh
    /// tail assistant placeholder with the active-turn pointer reparented
    /// onto it, chat status Thinking, bucket lifecycle Running.
    /// Reproduce-first: the pre-implementation stub reducer does none of it.
    #[test]
    fn cron_prompt_appended_appends_cron_block_and_opens_spinner() {
        use crate::app::session::SessionLifecycleState;
        use crate::app::{AppStatus, MessageBlock, MessageRole};

        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        // Model an idle, ready active session (a prior turn completed).
        app.status = AppStatus::Ready;
        if let Some(b) = app.sessions.get_mut(&key_a) {
            b.lifecycle_state = SessionLifecycleState::Idle;
        }
        app.clear_active_turn_assistant();

        apply_session_update(
            &mut app,
            SessionUpdate::CronPromptAppended {
                key: key_a.clone(),
                text: "run the morning summary".to_owned(),
            },
        );

        // A cron block landed: a User turn carrying the fired prompt,
        // flagged as a cron envelope (drives the distinct `Cron` label).
        let cron_msg = app
            .messages()
            .expect("active session")
            .iter()
            .find(|m| matches!(m.role, MessageRole::User) && m.is_cron_envelope)
            .expect("a cron-envelope user turn was appended");
        assert!(
            cron_msg.blocks.iter().any(|b| matches!(
                b, MessageBlock::Text(t) if t.text.contains("run the morning summary")
            )),
            "the cron block carries the fired prompt text",
        );

        // The delivered-turn spinner opens: a fresh empty assistant
        // placeholder at the tail with the active-turn pointer bound to it.
        let tail = app.messages().expect("active session").len() - 1;
        assert!(
            matches!(app.messages().expect("active session")[tail].role, MessageRole::Assistant)
                && app.messages().expect("active session")[tail].blocks.is_empty(),
            "a fresh empty assistant placeholder opens at the tail for the spinner",
        );
        assert_eq!(
            app.active_turn_assistant_message_idx(),
            Some(tail),
            "pointer targets the tail placeholder so the spinner pins to the bottom",
        );
        assert!(matches!(app.status, AppStatus::Thinking), "chat status flips to Thinking");
        assert_eq!(
            app.sessions.get(&key_a).expect("bucket").lifecycle_state,
            SessionLifecycleState::Running,
            "the Projects-pane row spins while the agent works the cron prompt",
        );
    }

    /// The `SlackMessageAppended` reducer surfaces an inbound Slack message:
    /// a User turn carrying the prose the session's LLM received, flagged
    /// `is_slack_envelope` so the role label reads `Slack` and the block
    /// renderer paints it. Reproduce-first: without the reducer the live
    /// delivery paints nothing.
    #[test]
    fn slack_message_appended_appends_a_slack_block() {
        use crate::app::{MessageBlock, MessageRole};

        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        let prose = "[Slack - workspace 'Trust Machines', granite-staging-alerts] id C0AE ts 1789.5\nunknown: _Large STX Transfer_";

        apply_session_update(
            &mut app,
            SessionUpdate::SlackMessageAppended { key: key_a.clone(), prose: prose.to_owned() },
        );

        let slack_msg = app
            .messages()
            .expect("active session")
            .iter()
            .find(|m| matches!(m.role, MessageRole::User) && m.is_slack_envelope)
            .expect("a Slack-envelope user turn was appended");
        assert!(
            slack_msg.blocks.iter().any(|b| matches!(b, MessageBlock::Text(t) if t.text == prose)),
            "the block carries the prose the session's LLM received",
        );
    }

    /// Single-session focused twin of
    /// [`background_event_updates_target_session_only`]: with only one
    /// real session in the map, an event tagged for the active key
    /// must still flip `needs_redraw`. Guards the routing rule
    /// (active-target events trigger redraw) without the multi-session
    /// noise of [`status_snapshot_routes_to_active_session_and_flips_redraw`].
    #[test]
    fn active_session_event_flips_needs_redraw() {
        let mut app = App::test_default();
        let key_a = SessionSlot::from_str_for_test("a");
        app.sessions.insert(key_a.clone(), UiSession::new(key_a.clone(), "test-project"));
        if let Some(ws) = app.workspace.as_ref() {
            let (h, _) = forge_workspace::Workspace::testing_stub_handle();
            let dom = ws.register_domain_session(key_a.clone(), Some(std::sync::Arc::new(h)));
            dom.lock().session_id = Some(forge_primitives::SessionId::new("a"));
        }
        app.active_session_key = Some(key_a.clone());
        app.needs_redraw = false;

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                key: SessionSlot::from_str_for_test("a"),
                account: forge_primitives::AccountInfo::default(),
                forge_account: None,
            },
        );

        assert!(app.needs_redraw, "active-session events must flip needs_redraw");
    }

    fn make_creds() -> forge_primitives::cloud::oauth_credentials::OauthCredentials {
        forge_primitives::cloud::oauth_credentials::OauthCredentials {
            access_token: "tok".to_owned(),
            expires_at: None,
        }
    }

    #[test]
    fn oauth_credentials_snapshot_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::OauthCredentialsSnapshot {
                key: key_b.clone(),
                credentials: Some(make_creds()),
            },
        );
        assert!(app.sessions.get(&key_b).expect("b").oauth_credentials.is_some());
        assert!(app.sessions.get(&key_a).expect("a").oauth_credentials.is_none());
        assert!(!app.needs_redraw);
    }

    #[test]
    fn oauth_credentials_snapshot_for_active_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::OauthCredentialsSnapshot {
                key: key_a.clone(),
                credentials: Some(make_creds()),
            },
        );
        assert!(app.sessions.get(&key_a).expect("a").oauth_credentials.is_some());
        assert!(app.needs_redraw);
    }

    #[test]
    fn context_usage_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::ContextUsageSnapshot {
                key: key_b.clone(),
                percentage: Some(42),
                max_tokens: Some(200_000),
            },
        );
        assert_eq!(
            app.sessions.get(&key_b).expect("b").session_usage.context_usage_percent,
            Some(42),
        );
        assert!(app.sessions.get(&key_a).expect("a").session_usage.context_usage_percent.is_none());
        assert!(!app.needs_redraw);
    }

    #[test]
    fn context_usage_routes_to_active_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::ContextUsageSnapshot {
                key: key_a.clone(),
                percentage: Some(7),
                max_tokens: Some(200_000),
            },
        );
        assert_eq!(
            app.sessions.get(&key_a).expect("a").session_usage.context_usage_percent,
            Some(7),
        );
        assert!(app.needs_redraw);
    }

    #[test]
    fn mcp_snapshot_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        let servers = vec![forge_primitives::McpServerStatus {
            name: "test-mcp".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        }];
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::McpSnapshot {
                key: key_b.clone(),
                servers,
                error: None,
            },
        );
        assert_eq!(app.sessions.get(&key_b).expect("b").mcp.servers.len(), 1);
        assert!(app.sessions.get(&key_a).expect("a").mcp.servers.is_empty());
        assert!(!app.needs_redraw);
    }

    #[test]
    fn mcp_snapshot_routes_to_active_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        let servers = vec![forge_primitives::McpServerStatus {
            name: "test-mcp".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        }];
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::McpSnapshot {
                key: key_a.clone(),
                servers,
                error: None,
            },
        );
        assert_eq!(app.sessions.get(&key_a).expect("a").mcp.servers.len(), 1);
        assert!(app.needs_redraw);
    }

    #[test]
    fn mcp_operation_error_routes_to_target_session_not_focused() {
        // key_a is the focused session, key_b a background one. A
        // background session's MCP error must land on ITS bucket, not
        // corrupt the focused session's /mcp overlay state.
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        app.sessions.get_mut(&key_a).expect("a").mcp.in_flight = true;
        app.sessions.get_mut(&key_b).expect("b").mcp.in_flight = true;
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::McpOperationError {
                key: key_b.clone(),
                error: forge_primitives::McpOperationError {
                    server_name: Some("ctx7".into()),
                    operation: "reconnect".into(),
                    message: "boom".into(),
                },
            },
        );
        let b = app.sessions.get(&key_b).expect("b");
        assert!(!b.mcp.in_flight, "background session's in_flight cleared");
        assert!(b.mcp.last_error.is_some(), "background session's error recorded");
        let a = app.sessions.get(&key_a).expect("a");
        assert!(a.mcp.in_flight, "focused session's mcp state untouched");
        assert!(a.mcp.last_error.is_none(), "focused session gets no stray error");
    }

    #[test]
    fn hook_observation_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::HookObservation {
                key: key_b.clone(),
                tool_use_id: Some("tool-1".into()),
                permission_mode: Some("acceptEdits".into()),
                effort: Some("max".into()),
                agent_id: Some("agent-1".into()),
                agent_type: Some("general-purpose".into()),
            },
        );
        let b = app.sessions.get(&key_b).expect("b");
        assert!(b.observed_permission_mode.is_some());
        assert!(b.observed_effort.is_some());
        assert_eq!(
            b.subagent_attribution.get("tool-1").map(String::as_str),
            Some("general-purpose")
        );
        let a = app.sessions.get(&key_a).expect("a");
        assert!(a.observed_permission_mode.is_none());
        assert!(a.observed_effort.is_none());
        assert!(a.subagent_attribution.is_empty());
        assert!(!app.needs_redraw);
    }

    #[test]
    fn hook_observation_routes_to_active_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::HookObservation {
                key: key_a.clone(),
                tool_use_id: None,
                permission_mode: Some("plan".into()),
                effort: None,
                agent_id: None,
                agent_type: None,
            },
        );
        assert!(app.sessions.get(&key_a).expect("a").observed_permission_mode.is_some());
        assert!(app.needs_redraw);
    }

    /// The hook-observed mirrors die with the session: a terminal
    /// connection failure must not leave the dead run's mode/effort
    /// chips rendered after a reconnect, until the next hook fires.
    #[test]
    fn connection_failed_clears_the_hook_observed_mirrors() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::HookObservation {
                key: key_a.clone(),
                tool_use_id: None,
                permission_mode: Some("acceptEdits".into()),
                effort: Some("max".into()),
                agent_id: None,
                agent_type: None,
            },
        );
        let a = app.sessions.get(&key_a).expect("a");
        assert!(a.observed_permission_mode.is_some() && a.observed_effort.is_some());

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::ConnectionFailed {
                key: key_a.clone(),
                message: "bridge down".into(),
                fatal: false,
            },
        );

        let a = app.sessions.get(&key_a).expect("a");
        assert!(a.observed_permission_mode.is_none(), "dead run's mode chip must not survive");
        assert!(a.observed_effort.is_none(), "dead run's effort chip must not survive");
    }

    #[test]
    fn unknown_session_event_drops_cleanly() {
        let mut app = App::test_default();
        let (_key_a, _key_b) = seed_two_sessions(&mut app);
        let unknown = SessionSlot::from_str_for_test("nope");
        let account = forge_primitives::AccountInfo {
            email: Some("ghost@example.com".to_owned()),
            ..Default::default()
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                key: unknown.clone(),
                account,
                forge_account: None,
            },
        );
        // Sessions A and B both unaffected; redraw flag stays false;
        // the unknown key must NOT have been silently inserted.
        assert!(!app.needs_redraw);
        assert!(
            !app.sessions.contains_key(&unknown),
            "unknown session key must not be inserted by the routing path"
        );
    }

    /// `SessionUpdate::Spawning` should seed a placeholder bucket
    /// under the key the spawn announced, with `Spawning` lifecycle
    /// state and a "Waking …" system message. Focus is the click's to move (via
    /// `pending_spawn_focus`) or the boot project's when the CLI named
    /// one; a wake nobody asked for registers in the background, so
    /// nothing here takes the tab.
    #[test]
    fn spawning_reducer_seeds_the_placeholder_bucket() {
        let mut app = App::test_default();
        // Strip the seeded test bucket so the assertions are clean.
        app.sessions.clear();
        app.active_session_key = None;

        let session_key = SessionSlot::from_str_for_test("forge-session-uuid".to_owned());
        // Simulate the workspace's spawn-path: it would normally
        // register a DomainSession under `session_key` before emitting
        // the SessionUpdate::Spawning. Tests bypass the workspace
        // spawn path and synthesize the SessionUpdate directly, so
        // pre-register the domain handle here to mirror production.
        {
            let ws = app.workspace.as_ref().expect("workspace stub present in test_default");
            let (stub_handle, _) = forge_workspace::Workspace::testing_stub_handle();
            ws.register_domain_session(session_key.clone(), Some(std::sync::Arc::new(stub_handle)));
        }

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: session_key.clone(),
                project_name: "forge".to_owned(),
                cwd: "/Users/v/Projects/forge".to_owned(),
                display_name: "forge".to_owned(),
            },
        );

        let bucket = app.sessions.get(&session_key).expect("spawn bucket created");
        assert!(
            matches!(bucket.lifecycle_state, crate::app::session::SessionLifecycleState::Spawning),
            "lifecycle state set to Spawning, got {:?}",
            bucket.lifecycle_state,
        );
        assert_eq!(bucket.cwd_raw, "/Users/v/Projects/forge");
        assert!(
            bucket.messages.iter().any(|m| matches!(m.role, crate::app::MessageRole::System(_))),
            "spawning placeholder system message present"
        );
        assert!(
            app.active_session_key.is_none(),
            "a wake nobody asked for must not take a tab it was not given",
        );
    }

    /// After the spawning reducer runs, `is_animating` (the
    /// render-loop probe) sees the session in `Spawning` lifecycle and
    /// keeps the spinner ticking.
    #[test]
    fn spinner_animates_during_spawning() {
        let mut app = App::test_default();
        app.sessions.clear();
        app.active_session_key = None;

        let session_key = SessionSlot::from_str_for_test("forge-session-uuid".to_owned());
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: session_key.clone(),
                project_name: "forge".to_owned(),
                cwd: "/Users/v/Projects/forge".to_owned(),
                display_name: "forge".to_owned(),
            },
        );

        let any_spawning_or_running = app.sessions.values().any(|s| {
            matches!(
                s.lifecycle_state,
                crate::app::session::SessionLifecycleState::Running
                    | crate::app::session::SessionLifecycleState::Spawning
            )
        });
        assert!(
            any_spawning_or_running,
            "Spawning bucket should drive the spinner via direct UiSession read",
        );
    }

    /// `SessionUpdate::Spawning` writes `cwd_raw` directly onto the
    /// bucket, which is the sole owner.
    #[test]
    fn spawning_reducer_writes_cwd_raw_onto_bucket() {
        let mut app = App::test_default();
        app.sessions.clear();
        app.active_session_key = None;
        let key = SessionSlot::from_str_for_test("a-session-uuid".to_owned());
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: key.clone(),
                project_name: "a".to_owned(),
                cwd: "/p/a".to_owned(),
                display_name: "a".to_owned(),
            },
        );
        let bucket = app.sessions.get(&key).expect("bucket created");
        assert_eq!(bucket.cwd_raw, "/p/a");
    }

    /// `SessionUpdate::StatusSnapshot` for a background session writes
    /// `account_info` + `active_account_display_name` directly onto
    /// the target bucket without touching the active session's bucket.
    #[test]
    fn status_snapshot_background_writes_account_fields_to_bucket() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        let account = forge_primitives::AccountInfo {
            email: Some("bg@example.com".to_owned()),
            ..Default::default()
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                key: background.clone(),
                account: account.clone(),
                forge_account: Some(forge_primitives::ForgeAccountIdentity::new(
                    "Background".to_owned(),
                )),
            },
        );
        assert!(bucket_account_info_for(&app, &background).is_some());
        assert_eq!(
            app.sessions.get(&background).and_then(|s| s.active_account_display_name.as_deref()),
            Some("Background"),
        );
        // Active bucket left untouched.
        assert!(bucket_account_info_for(&app, &active).is_none());
    }

    /// A background `Connected` refreshes a connecting bucket's presentation
    /// and must leave its background roster standing. `task_started` is not
    /// re-emitted for a task that is still running, so a record cleared here
    /// could never be re-earned: a live bash would lose both its PROCESSES row
    /// and its spinner. Only the replacement path clears, because only there is
    /// the chat the roster described actually dropped.
    #[test]
    fn connected_background_leaves_the_background_roster_standing() {
        let mut app = App::test_default();
        let (_active, background) = seed_two_sessions(&mut app);
        {
            let bucket = app.sessions.get_mut(&background).expect("bucket");
            bucket.background_tasks.push(crate::app::BackgroundTask {
                task_id: "task-bash".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "watch CI".to_owned(),
            });
            bucket.session_task_tool_use_ids.insert(
                "task-bash".to_owned(),
                crate::app::SessionTaskCard {
                    tool_use_id: "tu-bash".to_owned(),
                    card_seen: true,
                    command: Some("gh run watch 123".to_owned()),
                },
            );
        }
        assert!(
            app.sessions.get(&background).expect("bucket").has_live_background_work(),
            "precondition: the roster drives the row",
        );

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Connected {
                key: background.clone(),
                session_id: forge_primitives::SessionId::new(background.display()),
                cwd: "/bg".to_owned(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        let bucket = app.sessions.get(&background).expect("bucket present");
        assert!(
            bucket.background_tasks.iter().any(|task| task.task_id == "task-bash"),
            "a connected bucket keeps the roster it already had",
        );
        assert!(
            bucket.session_task_tool_use_ids.contains_key("task-bash"),
            "and the card recorded with it, which cannot be re-earned",
        );
        assert!(bucket.has_live_background_work(), "so the row keeps spinning");
    }

    /// `SessionUpdate::Connected` on a background bucket writes
    /// session_id + cwd_raw + lifecycle Idle directly onto the bucket
    /// while also mirroring session_id onto the workspace's
    /// DomainSession for AgentHandle dispatch.
    #[test]
    fn connected_background_writes_session_id_onto_bucket_and_domain() {
        let mut app = App::test_default();
        let (_active, background) = seed_two_sessions(&mut app);
        // Clear the bucket's session_id so we can verify the reducer
        // re-stamps it.
        if let Some(b) = app.sessions.get_mut(&background) {
            b.session_id = None;
            b.cwd_raw = String::new();
            b.lifecycle_state = crate::app::session::SessionLifecycleState::Spawning;
        }
        let current_model = forge_primitives::CurrentModel {
            resolved_id: "claude".to_owned(),
            display_name_short: "claude".to_owned(),
            display_name_long: "claude".to_owned(),
            requested_id: None,
            catalog_id: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_auto_mode: None,
            supports_adaptive_thinking: None,
            is_authoritative: true,
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Connected {
                key: background.clone(),
                session_id: forge_primitives::SessionId::new(background.display()),
                cwd: "/bg".to_owned(),
                current_model,
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );
        let bucket = app.sessions.get(&background).expect("bucket present");
        assert_eq!(bucket.cwd_raw, "/bg");
        assert!(matches!(bucket.lifecycle_state, crate::app::session::SessionLifecycleState::Idle));
        assert_eq!(
            bucket.session_id.as_ref().map(std::string::ToString::to_string),
            Some(background.display()),
        );
        // The reducer also mirrors session_id onto the workspace's
        // DomainSession so AgentHandle dispatch routes through the
        // claude-issued UUID. Without this, `Command::Cancel` /
        // `Prompt` for the background session would carry the wrong
        // (or no) session_id when SessionTask processes them.
        let domain = app
            .workspace
            .as_ref()
            .and_then(|ws| ws.domain_session_for(&background))
            .expect("domain registered by seed_two_sessions");
        let domain_sid = domain.lock().session_id.as_ref().map(std::string::ToString::to_string);
        assert_eq!(domain_sid, Some(background.display()));
    }

    fn test_mcp_server() -> forge_primitives::McpServerStatus {
        forge_primitives::McpServerStatus {
            name: "test-mcp".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        }
    }

    /// A background session's `Connected` must run its post-connect
    /// refreshes against ITS bucket. Before the pivot they ran against
    /// whatever was focused: `refresh_mcp_snapshot` wiped the focused
    /// session's server list (emptying the `/mcp` overlay, and dropping
    /// the Inspector PROCESSES resolver back to package-derived MCP
    /// names) while the connecting session's own list never populated.
    #[test]
    fn background_connected_refreshes_target_session_not_focused() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        // `seed_two_sessions` drops its stub command receivers, which makes
        // every dispatch fail; live ones let the refresh actually land.
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);
        let _cmds_background = workspace.install_testing_stub(&background);
        app.sessions.get_mut(&active).expect("a").mcp.servers = vec![test_mcp_server()];

        apply_session_update(
            &mut app,
            SessionUpdate::Connected {
                key: background.clone(),
                session_id: forge_primitives::SessionId::new(background.display()),
                cwd: "/bg".to_owned(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        let focused = app.sessions.get(&active).expect("a");
        assert_eq!(focused.mcp.servers.len(), 1, "focused session's MCP list must survive");
        assert!(!focused.mcp.in_flight, "focused session must not be re-fetched");
        assert!(
            !focused.session_usage.context_usage_in_flight,
            "focused session's context usage must not be re-fetched",
        );

        let connecting = app.sessions.get(&background).expect("b");
        assert!(connecting.mcp.in_flight, "connecting session requests its own MCP snapshot");
        assert!(
            connecting.session_usage.context_usage_in_flight,
            "connecting session requests its own context usage",
        );
    }

    /// A success `Message::Result` frame is the production shape of
    /// "the turn completed" - nothing constructs
    /// `SessionUpdate::TurnComplete` - so the background dispatch is
    /// the writer that must arm the unseen-completion flag.
    #[test]
    fn background_success_result_arms_unseen_completion() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);
        let _cmds_background = workspace.install_testing_stub(&background);

        apply_session_update(
            &mut app,
            SessionUpdate::ChatAppended {
                key: background.clone(),
                msg: result_frame(&background.display(), false),
            },
        );

        assert!(
            app.sessions.get(&background).expect("bg bucket").unseen_turn_completion,
            "a background success Result must arm the unseen-completion flag",
        );
        assert!(
            !app.sessions.get(&active).expect("active bucket").unseen_turn_completion,
            "the watched session must stay clean",
        );
    }

    /// The production shape of a background completion - a success
    /// `Message::Result` on a non-active bucket - raises the
    /// completion ping. The reducer's background arm is unreachable
    /// under the pivot, so the dispatcher seam is the notify site,
    /// beside the unseen-completion write it already owns.
    #[test]
    fn background_success_result_notifies_when_unfocused() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);
        let _cmds_background = workspace.install_testing_stub(&background);
        if let Some(bucket) = app.sessions.get_mut(&background) {
            bucket.project = "beta".to_owned();
        }
        app.notifications.on_focus_lost();

        apply_session_update(
            &mut app,
            SessionUpdate::ChatAppended {
                key: background.clone(),
                msg: result_frame(&background.display(), false),
            },
        );

        assert_eq!(
            crate::app::notify::test_capture::take_notifications(&app),
            vec![(
                crate::app::notify::NotifyEvent::TurnComplete,
                crate::app::notify::NotifyContext {
                    project: "beta".to_owned(),
                    worker_label: None,
                },
            )],
            "a background success Result pings with that session's project",
        );
    }

    /// When the focused tab is mid-turn, the pivot runs the active arm
    /// whose tail notify would fire with the background key. The seam
    /// is the one writer: exactly one completion ping per background
    /// Result, never two.
    #[test]
    fn background_result_notifies_once_when_the_focused_tab_is_busy() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);
        let _cmds_background = workspace.install_testing_stub(&background);
        if let Some(bucket) = app.sessions.get_mut(&background) {
            bucket.project = "beta".to_owned();
        }
        app.status = crate::app::AppStatus::Thinking;
        app.notifications.on_focus_lost();

        apply_session_update(
            &mut app,
            SessionUpdate::ChatAppended {
                key: background.clone(),
                msg: result_frame(&background.display(), false),
            },
        );

        assert_eq!(
            crate::app::notify::test_capture::take_notifications(&app),
            vec![(
                crate::app::notify::NotifyEvent::TurnComplete,
                crate::app::notify::NotifyContext {
                    project: "beta".to_owned(),
                    worker_label: None,
                },
            )],
            "one background completion, one ping - the pivot tail must not double it",
        );
    }

    /// The restore half of the pivot marker: once a background frame
    /// has been routed, the marker must be cleared again, or the tail
    /// notify dies for the user's own tab. One background success
    /// Result, then a turn completing on the truly active tab, must
    /// leave exactly one ping per session.
    #[test]
    fn active_tab_pings_after_a_background_frame_routes_through_the_pivot() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);
        let _cmds_background = workspace.install_testing_stub(&background);
        if let Some(bucket) = app.sessions.get_mut(&active) {
            bucket.project = "alpha".to_owned();
        }
        if let Some(bucket) = app.sessions.get_mut(&background) {
            bucket.project = "beta".to_owned();
        }
        app.notifications.on_focus_lost();

        apply_session_update(
            &mut app,
            SessionUpdate::ChatAppended {
                key: background.clone(),
                msg: result_frame(&background.display(), false),
            },
        );
        app.status = crate::app::AppStatus::Thinking;
        turn::apply_session_update_turn_complete(&mut app, &active, None);

        assert_eq!(
            crate::app::notify::test_capture::take_notifications(&app),
            vec![
                (
                    crate::app::notify::NotifyEvent::TurnComplete,
                    crate::app::notify::NotifyContext {
                        project: "beta".to_owned(),
                        worker_label: None,
                    },
                ),
                (
                    crate::app::notify::NotifyEvent::TurnComplete,
                    crate::app::notify::NotifyContext {
                        project: "alpha".to_owned(),
                        worker_label: None,
                    },
                ),
            ],
            "one ping per completion: the background seam's, then the active tab's \
             own - a marker left stuck suppresses the second",
        );
    }

    /// The active session's own success Result is seen by definition:
    /// the flag must not arm on the watched session.
    #[test]
    fn active_success_result_arms_nothing() {
        let mut app = App::test_default();
        let (active, _background) = seed_two_sessions(&mut app);
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);

        apply_session_update(
            &mut app,
            SessionUpdate::ChatAppended {
                key: active.clone(),
                msg: result_frame(&active.display(), false),
            },
        );

        assert!(
            !app.sessions.get(&active).expect("bucket").unseen_turn_completion,
            "a turn completing on the watched session must not arm the flag",
        );
    }

    /// REPRODUCTION for the wrong-title report: a background completion
    /// in project "hub-modules" while the active tab is "core-v1" must
    /// notify with the COMPLETING session's project in the title.
    #[test]
    fn background_completion_notifies_with_the_completing_sessions_project() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        // Distinct projects: the active tab is core-v1, the background
        // session belongs to hub-modules.
        app.sessions.get_mut(&active).expect("active").project = "core-v1".to_owned();
        app.sessions.get_mut(&background).expect("bg").project = "hub-modules".to_owned();
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);
        let _cmds_background = workspace.install_testing_stub(&background);

        apply_session_update(
            &mut app,
            SessionUpdate::ChatAppended {
                key: background.clone(),
                msg: result_frame(&background.display(), false),
            },
        );

        let captured = crate::app::notify::test_capture::take_notifications(&app);
        let fired: Vec<_> = captured
            .iter()
            .filter(|(event, _)| *event == crate::app::notify::NotifyEvent::TurnComplete)
            .collect();
        assert_eq!(fired.len(), 1, "exactly one TurnComplete notification: {captured:?}");
        assert_eq!(
            fired[0].1.project.as_str(),
            "hub-modules",
            "the title must name the COMPLETING session's project, got {captured:?}"
        );
    }

    /// Only a success Result arms the unseen-completion flag: the
    /// failed shape routes through the turn-error handlers and leaves
    /// the flag alone.
    #[test]
    fn failed_result_does_not_arm_unseen_completion() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);
        let _cmds_background = workspace.install_testing_stub(&background);

        apply_session_update(
            &mut app,
            SessionUpdate::ChatAppended {
                key: background.clone(),
                msg: result_frame(&background.display(), true),
            },
        );

        assert!(
            !app.sessions.get(&background).expect("bg bucket").unseen_turn_completion,
            "a failed Result must not arm the unseen-completion flag",
        );
        assert!(
            !app.sessions.get(&active).expect("active bucket").unseen_turn_completion,
            "no other bucket is touched either",
        );
    }

    /// The focused session's own `Connected` still refreshes it - the
    /// pivot must not skip the active case.
    #[test]
    fn active_connected_refreshes_its_own_session() {
        let mut app = App::test_default();
        let (active, _background) = seed_two_sessions(&mut app);
        let workspace = app.workspace.clone().expect("workspace");
        let _cmds_active = workspace.install_testing_stub(&active);
        app.sessions.get_mut(&active).expect("a").mcp.servers = vec![test_mcp_server()];

        apply_session_update(
            &mut app,
            SessionUpdate::Connected {
                key: active.clone(),
                session_id: forge_primitives::SessionId::new(active.display()),
                cwd: "/fg".to_owned(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        let focused = app.sessions.get(&active).expect("a");
        assert!(focused.mcp.servers.is_empty(), "its own connect clears the stale list");
        assert!(focused.mcp.in_flight, "and requests a fresh snapshot");
        assert_eq!(app.active_session_key.as_ref(), Some(&active), "focus stays put");
    }

    /// `SessionUpdate::Spawning` should be idempotent: a second
    /// Spawning for the same key must NOT reset the bucket. In
    /// production the repeat arrives from a background wake (cron,
    /// peer prompt, gotify or slack delivery); once the stub exists, a duplicate click is
    /// refused by the click handler. Bucket state is preserved and
    /// focus stays where the user put it.
    #[test]
    fn spawning_reducer_is_idempotent_for_repeat_keys() {
        let mut app = App::test_default();
        app.sessions.clear();
        let active_before = app.active_session_key.clone();
        let key = SessionSlot::from_str_for_test("proj-session-uuid".to_owned());
        // First Spawning seeds the bucket.
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: key.clone(),
                project_name: "proj".to_owned(),
                cwd: "/proj".to_owned(),
                display_name: "proj".to_owned(),
            },
        );
        let messages_after_first = app.sessions.get(&key).expect("bucket").messages.len();
        // State accumulates between the two wakes; the reducer must
        // not re-seed over it.
        if let Some(b) = app.sessions.get_mut(&key) {
            b.messages.push(crate::app::ChatMessage::new(
                crate::app::MessageRole::System(Some(crate::app::SystemSeverity::Info)),
                vec![crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(
                    "intermediate state",
                ))],
            ));
            b.message_retained_bytes.push(0);
        }
        let messages_after_second_state = app.sessions.get(&key).expect("bucket").messages.len();
        assert!(messages_after_second_state > messages_after_first);
        // Second Spawning for the same key (a background repeat).
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: key.clone(),
                project_name: "proj".to_owned(),
                cwd: "/proj".to_owned(),
                display_name: "proj".to_owned(),
            },
        );
        // Bucket state preserved (idempotent - no re-seed).
        assert_eq!(
            app.sessions.get(&key).expect("bucket").messages.len(),
            messages_after_second_state,
        );
        // The repeat wake registers without taking the tab: no click
        // asked for this key while its stub already existed.
        assert_eq!(app.active_session_key, active_before, "focus stays put");
    }

    /// Waking a cold project from the Projects pane records the project
    /// it was headed for, and the reducer honours it when the bucket
    /// appears. Without the hand-off the click dispatches a spawn and
    /// leaves the user exactly where they were, which is
    /// indistinguishable from a dead row.
    #[test]
    fn spawning_follows_the_focus_a_user_click_asked_for() {
        let mut app = App::test_default();
        let elsewhere = SessionSlot::from_str_for_test("some-other-session");
        app.sessions.insert(
            elsewhere.clone(),
            crate::app::session::UiSession::new(elsewhere.clone(), "test-project"),
        );
        app.active_session_key = Some(elsewhere);

        let key = SessionSlot::from_str_for_test("cold-uuid".to_owned());
        app.pending_spawn_focus = Some("cold".to_owned());
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: key.clone(),
                project_name: "cold".to_owned(),
                cwd: "/p/cold".to_owned(),
                display_name: "cold".to_owned(),
            },
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&key),
            "a click-driven wake must land the user in the session it woke",
        );
        assert!(
            app.pending_spawn_focus.is_none(),
            "the intent is one-shot; leaving it set would re-steal focus on a later spawn",
        );
    }

    /// The recorded intent is a request, not a reservation: if the
    /// user goes somewhere else before the spawn lands, the spawn must
    /// not pull them back out of the session they chose second.
    #[test]
    fn switching_away_before_the_spawn_lands_abandons_the_pending_focus() {
        let mut app = App::test_default();
        let chosen = SessionSlot::from_str_for_test("session-picked-instead");
        app.sessions.insert(
            chosen.clone(),
            crate::app::session::UiSession::new(chosen.clone(), "test-project"),
        );

        let waking = SessionSlot::from_str_for_test("cold-uuid".to_owned());
        app.pending_spawn_focus = Some("cold".to_owned());
        app.switch_active_session(chosen.clone());
        assert!(
            app.pending_spawn_focus.is_none(),
            "landing somewhere else must abandon the earlier request",
        );

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: waking,
                project_name: "cold".to_owned(),
                cwd: "/p/cold".to_owned(),
                display_name: "cold".to_owned(),
            },
        );
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&chosen),
            "the abandoned spawn must not yank focus back when it arrives",
        );
    }

    /// The negative control for the test above, and the reason the
    /// reducer cannot simply always focus: every `auto_start` project
    /// emits `Spawning` at boot, so an unconditional switch hands the
    /// tab to whichever one the scheduler happens to run first.
    #[test]
    fn spawning_leaves_focus_alone_when_no_click_asked_for_it() {
        let mut app = App::test_default();
        let watching = SessionSlot::from_str_for_test("session-the-user-is-reading");
        app.sessions.insert(
            watching.clone(),
            crate::app::session::UiSession::new(watching.clone(), "test-project"),
        );
        app.active_session_key = Some(watching.clone());
        assert!(app.pending_spawn_focus.is_none(), "no click preceded this spawn");

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: SessionSlot::from_str_for_test("autostart-session-uuid".to_owned()),
                project_name: "autostart".to_owned(),
                cwd: "/p/autostart".to_owned(),
                display_name: "autostart".to_owned(),
            },
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&watching),
            "an unasked-for spawn must not take the tab the user is reading",
        );
    }

    /// A launchpad boot names no project, so the `auto_start` spawns it
    /// dispatches land in the background while the picker holds the
    /// screen. Nothing may take the tab until the user picks one.
    #[test]
    fn a_launchpad_boot_leaves_an_auto_start_spawn_in_the_background() {
        let mut app = App::test_default();
        app.sessions.clear();
        app.active_session_key = None;
        app.startup_project = None;

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: SessionSlot::from_str_for_test("autostart-session-uuid".to_owned()),
                project_name: "autostart".to_owned(),
                cwd: "/p/autostart".to_owned(),
                display_name: "autostart".to_owned(),
            },
        );

        assert!(
            app.active_session_key.is_none(),
            "the picker keeps the screen until the user picks a project",
        );
    }

    /// The same rule on the arm that finds a bucket already there: a
    /// wake reusing the stub an earlier failed spawn left behind must
    /// not take the tab either, or the picker loses its screen to
    /// whatever `auto_start` project the scheduler runs first.
    #[test]
    fn a_launchpad_boot_leaves_a_stub_wake_in_the_background() {
        let mut app = App::test_default();
        app.sessions.clear();
        app.active_session_key = None;
        app.startup_project = None;
        let key = SessionSlot::from_str_for_test("autostart-session-uuid".to_owned());
        app.sessions
            .insert(key.clone(), crate::app::session::UiSession::new(key.clone(), "autostart"));

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: key.clone(),
                project_name: "autostart".to_owned(),
                cwd: "/p/autostart".to_owned(),
                display_name: "autostart".to_owned(),
            },
        );

        assert!(
            app.active_session_key.is_none(),
            "a wake reusing a stale stub must not take the picker's screen",
        );
    }

    /// The chat-direct boot path: `StartDefault` emits no `Spawning`, so
    /// `Connected` is the only event the TUI sees, and the session the
    /// user launched forge for has to take the tab.
    /// Without that the chat renders empty for it, `App.status` stays
    /// `Connecting` because the status mirror has no bucket to read, and
    /// the render loop animates a session nobody can see.
    #[test]
    fn a_boot_connect_takes_the_tab_when_nothing_is_focused() {
        let mut app = App::test_default();
        app.sessions.clear();
        app.active_session_key = None;
        app.startup_project = Some("boot-proj".to_owned());
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project("boot-proj", "/tmp/boot-proj");

        let real = SessionSlot::from_str_for_test("real-uuid");
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Connected {
                key: real.clone(),
                session_id: forge_primitives::SessionId::new("real-uuid"),
                cwd: "/tmp/boot-proj".to_owned(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&real),
            "the boot session takes the tab nothing else is holding",
        );
        assert!(app.sessions.contains_key(&real), "and it is the session the accessors read");
    }

    /// A background spawn wake (cron, peer prompt, gotify or slack delivery) landing while the
    /// user's own click-woken spawn is mid-boot must not steal the
    /// landing. Project B's earlier spawn failed and left its stub
    /// bucket behind; when B is woken again in the background, the
    /// existing-bucket branch used to switch focus unconditionally - so
    /// the click on A landed the user on B's stub instead, and a second
    /// click was needed to enter A.
    #[test]
    fn background_spawn_wake_does_not_hijack_the_clicked_projects_landing() {
        let mut app = App::test_default();
        app.sessions.clear();

        // The user clicked project A; its Spawning honored the pending
        // focus and sits mid-boot on the waking stub.
        let clicked = SessionSlot::from_str_for_test("wake-a-uuid".to_owned());
        app.pending_spawn_focus = Some("a".to_owned());
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: clicked.clone(),
                project_name: "a".to_owned(),
                cwd: "/p/a".to_owned(),
                display_name: "a".to_owned(),
            },
        );
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&clicked),
            "precondition: the click's wake took the tab",
        );

        // Project B failed to spawn earlier; its stub survived. A cron,
        // peer prompt, gotify or slack delivery wakes B in the background
        // during A's boot window.
        let stale = SessionSlot::from_str_for_test("stale-b-uuid".to_owned());
        app.sessions.insert(stale.clone(), crate::app::session::UiSession::new(stale.clone(), "b"));
        app.needs_redraw = false;
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: stale.clone(),
                project_name: "b".to_owned(),
                cwd: "/p/b".to_owned(),
                display_name: "b".to_owned(),
            },
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&clicked),
            "a background wake must not move focus off the clicked spawn",
        );
        assert!(app.needs_redraw, "the declined wake must still repaint the pane");
    }

    /// The pending focus must survive a declined background wake. A
    /// mutant that clears the flag unconditionally in the
    /// existing-bucket branch passes every focus assertion - the
    /// user's click would then never land, with no test the wiser.
    #[test]
    fn declined_background_wake_preserves_the_pending_focus() {
        let mut app = App::test_default();
        app.sessions.clear();

        // The user clicked cold project A: its intent is armed but the
        // bucket has not appeared yet. Project B's stub exists.
        let clicked = SessionSlot::from_str_for_test("wake-a-uuid".to_owned());
        app.pending_spawn_focus = Some("a".to_owned());
        let stale = SessionSlot::from_str_for_test("stale-b-uuid".to_owned());
        app.sessions.insert(stale.clone(), crate::app::session::UiSession::new(stale.clone(), "b"));

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: stale.clone(),
                project_name: "b".to_owned(),
                cwd: "/p/b".to_owned(),
                display_name: "b".to_owned(),
            },
        );
        assert_eq!(
            app.pending_spawn_focus.as_deref(),
            Some("a"),
            "a declined background wake must not consume the click's intent",
        );

        // A's own wake arrives and must still take the tab.
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: clicked.clone(),
                project_name: "a".to_owned(),
                cwd: "/p/a".to_owned(),
                display_name: "a".to_owned(),
            },
        );
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&clicked),
            "the preserved intent must still land the user on its wake",
        );
    }

    /// Build a minimal `forge_primitives::CurrentModel` for tests
    /// that need to fire a `SessionUpdate::Connected` /
    /// `SessionUpdate::SessionReplaced` envelope. Field values are
    /// deliberately uninteresting - the assertion target is the
    /// file_index side effect, not the model state.
    fn test_current_model() -> forge_primitives::CurrentModel {
        forge_primitives::CurrentModel {
            requested_id: None,
            resolved_id: "test-model".to_owned(),
            display_name_short: "test-model".to_owned(),
            display_name_long: "test-model".to_owned(),
            catalog_id: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_auto_mode: None,
            supports_adaptive_thinking: None,
            is_authoritative: true,
        }
    }

    /// `SessionUpdate::Connected` for the active session reaches the
    /// active apply-chain path which restarts `app.file_index` with
    /// the new cwd. After the event lands, the file_index root must
    /// match the new cwd, the generation must have advanced, and the
    /// stale `entries` map must have been cleared so the next scan
    /// starts from a clean slate. The asynchronous scan completion
    /// itself isn't asserted - only that the synchronous restart side
    /// effects fired against the production reducer path.
    #[test]
    fn connected_refreshes_file_index_candidates_for_new_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonicalize");
        let mut app = App::test_default();
        // Seed stale file_index state to verify the restart wipes it.
        app.file_index_mut().expect("active session").generation = 3;
        app.file_index_mut().expect("active session").root =
            Some(std::path::PathBuf::from("/old/path"));
        app.file_index_mut().expect("active session").entries.insert(
            "stale.rs".to_owned(),
            crate::app::file_index::FileCandidate {
                rel_path: "stale.rs".to_owned(),
                rel_path_lower: "stale.rs".to_owned(),
                basename_lower: "stale.rs".to_owned(),
                depth: 0,
            },
        );
        app.file_index_mut().expect("active session").scan_finished = true;

        let pending_key = app.active_session_key.clone().expect("pending active key");
        let new_cwd = canonical.to_string_lossy().into_owned();

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Connected {
                key: pending_key,
                session_id: forge_primitives::SessionId::new("session-1"),
                cwd: new_cwd.clone(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        assert_eq!(
            app.file_index_mut().expect("active session").root.as_deref(),
            Some(canonical.as_path()),
            "file_index root must follow the Connected cwd",
        );
        assert!(
            app.file_index_mut().expect("active session").generation > 3,
            "file_index generation must advance on restart"
        );
        assert!(
            app.file_index_mut().expect("active session").entries.is_empty(),
            "stale entries cleared on restart"
        );
        assert!(
            !app.file_index_mut().expect("active session").scan_finished,
            "scan_finished reset on restart"
        );
    }

    /// `SessionUpdate::SessionReplaced` for the session on screen shares
    /// the `handle_session_replaced_event` path which restarts the
    /// `file_index` against the replaced cwd. Same assertion shape as
    /// the `Connected` test - production code path runs through
    /// `apply_session_update_session_replaced` →
    /// `handle_session_replaced_event` → `file_index::restart`. The
    /// background arm reaches the file index through its own bucket, so
    /// only the on-screen arm has app-level candidates to assert on.
    #[test]
    fn session_replaced_refreshes_file_index_candidates_for_replaced_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonicalize");
        let mut app = App::test_default();
        app.file_index_mut().expect("active session").generation = 8;
        app.file_index_mut().expect("active session").root =
            Some(std::path::PathBuf::from("/before"));
        app.file_index_mut().expect("active session").entries.insert(
            "before.rs".to_owned(),
            crate::app::file_index::FileCandidate {
                rel_path: "before.rs".to_owned(),
                rel_path_lower: "before.rs".to_owned(),
                basename_lower: "before.rs".to_owned(),
                depth: 0,
            },
        );
        app.file_index_mut().expect("active session").scan_finished = true;

        let pending_key = app.active_session_key.clone().expect("pending active key");
        let replaced_cwd = canonical.to_string_lossy().into_owned();

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::SessionReplaced {
                key: pending_key.clone(),
                session_id: forge_primitives::SessionId::new("session-2"),
                cwd: replaced_cwd.clone(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        assert_eq!(
            app.file_index_mut().expect("active session").root.as_deref(),
            Some(canonical.as_path()),
            "file_index root must follow the SessionReplaced cwd",
        );
        assert!(
            app.file_index_mut().expect("active session").generation > 8,
            "file_index generation must advance on restart"
        );
        assert!(
            app.file_index_mut().expect("active session").entries.is_empty(),
            "stale entries cleared on restart"
        );
        assert!(
            !app.file_index_mut().expect("active session").scan_finished,
            "scan_finished reset on restart"
        );
    }

    /// A replaced session that is not the one on screen takes the
    /// background arm, which never passes through `Connected`. With no
    /// bucket at the replaced key to carry a project across, the
    /// replacement is minted from its new cwd, which is the only thing
    /// naming the project it belongs to.
    #[test]
    fn background_session_replaced_stamps_the_project_from_the_new_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonicalize");
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project("resumed-proj", canonical.to_str().expect("utf-8 path"));

        // The user is on another tab, so the replaced key takes the
        // background arm rather than the on-screen one.
        app.active_session_key = Some(SessionSlot::from_str_for_test("on-screen"));

        let replacement = SessionSlot::from_str_for_test("new-uuid");
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::SessionReplaced {
                key: replacement.clone(),
                session_id: forge_primitives::SessionId::new("new-uuid"),
                cwd: canonical.to_string_lossy().into_owned(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        assert_eq!(
            app.sessions.get(&replacement).map(|b| b.project.as_str()),
            Some("resumed-proj"),
            "a background resume picks up the project its new cwd names",
        );
    }

    /// The background arm leaves the project of a bucket it found alone:
    /// the slot keeps its bucket, so its stamped name is the tab's
    /// identity and the replacement's cwd does not re-file it. The
    /// session_id + cwd assertions carry the non-vacuity - this arm does
    /// reach the bucket, so an untouched project is a decision, not a
    /// dropped frame.
    #[test]
    fn background_session_replaced_keeps_a_previously_stamped_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonicalize");
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project("resumed-proj", canonical.to_str().expect("utf-8 path"));

        app.active_session_key = Some(SessionSlot::from_str_for_test("on-screen"));

        let replacement = SessionSlot::from_str_for_test("new-uuid".to_owned());
        let mut bucket = UiSession::new(replacement.clone(), "test-project");
        bucket.project = "preset".to_owned();
        app.sessions.insert(replacement.clone(), bucket);

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::SessionReplaced {
                key: replacement.clone(),
                session_id: forge_primitives::SessionId::new("new-uuid"),
                cwd: canonical.to_string_lossy().into_owned(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        let bucket = app.sessions.get(&replacement).expect("the replaced slot keeps its bucket");
        assert_eq!(bucket.project, "preset", "a project already stamped survives the replacement");
        assert_eq!(
            bucket.cwd_raw,
            canonical.to_string_lossy(),
            "the replacement reached the bucket, so the untouched project is a decision",
        );
        assert_eq!(
            bucket.session_id.as_ref().map(ToString::to_string).as_deref(),
            Some("new-uuid"),
            "and the slot carries the new occupant",
        );
    }

    /// `SessionUpdate::PermissionRequest` enqueues a `PromptState`
    /// onto the target session's `prompt_queue`; the unified-prompt
    /// dock reads from that queue.
    #[test]
    fn permission_request_event_enqueues_prompt_on_target_session() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        let request = crate::app::prompt::tests::make_permission_request();
        apply_session_update(
            &mut app,
            SessionUpdate::PermissionRequest {
                key: key_a.clone(),
                tool_id: "tc-evt".into(),
                request,
            },
        );
        let session = app.sessions.get(&key_a).expect("session a");
        assert_eq!(session.prompt_queue.len(), 1, "prompt enqueued onto queue");
        assert_eq!(
            session.prompt_queue.front().expect("head").tool_id,
            "tc-evt",
            "queued prompt carries the event's tool_id"
        );
    }

    /// Same shape as the permission test for `QuestionRequest`.
    #[test]
    fn question_request_event_enqueues_prompt_on_target_session() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        let request = crate::app::prompt::tests::make_question_request(false);
        apply_session_update(
            &mut app,
            SessionUpdate::QuestionRequest {
                key: key_a.clone(),
                tool_id: "tc-q-evt".into(),
                request,
            },
        );
        let session = app.sessions.get(&key_a).expect("session a");
        assert_eq!(session.prompt_queue.len(), 1, "question prompt enqueued");
        assert_eq!(
            session.prompt_queue.front().expect("head").tool_id,
            "tc-q-evt",
            "queued question prompt carries the event's tool_id"
        );
    }

    /// A queued question's only other signal is a glyph on a row, so
    /// the enqueue has to raise the notification too. The variant
    /// existed with no production caller: its call sites went out with
    /// the dead pre-MVVM prompt path in #131 and never came back.
    #[test]
    fn question_request_event_raises_the_question_notification() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        if let Some(bucket) = app.sessions.get_mut(&key_a) {
            bucket.project = "alpha".to_owned();
        }
        apply_session_update(
            &mut app,
            SessionUpdate::QuestionRequest {
                key: key_a,
                tool_id: "tc-q-evt".into(),
                request: crate::app::prompt::tests::make_question_request(false),
            },
        );
        assert_eq!(
            crate::app::notify::test_capture::take_notifications(&app),
            vec![(
                crate::app::notify::NotifyEvent::QuestionRequired,
                crate::app::notify::NotifyContext {
                    project: "alpha".to_owned(),
                    worker_label: None,
                },
            )],
            "an enqueued question raises QuestionRequired",
        );
    }

    /// A permission request had the same silence as a question, on the
    /// adjacent arm of the same match.
    #[test]
    fn permission_request_event_raises_the_permission_notification() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        if let Some(bucket) = app.sessions.get_mut(&key_a) {
            bucket.project = "alpha".to_owned();
        }
        apply_session_update(
            &mut app,
            SessionUpdate::PermissionRequest {
                key: key_a,
                tool_id: "tc-evt".into(),
                request: crate::app::prompt::tests::make_permission_request(),
            },
        );
        assert_eq!(
            crate::app::notify::test_capture::take_notifications(&app),
            vec![(
                crate::app::notify::NotifyEvent::PermissionRequired,
                crate::app::notify::NotifyContext {
                    project: "alpha".to_owned(),
                    worker_label: None,
                },
            )],
            "an enqueued permission request raises PermissionRequired",
        );
    }

    /// The notification text resolves from the EVENT's session, not the
    /// active tab: a background worker's prompts name the worker's
    /// project and label while the user reads another session. Both
    /// prompt kinds, since seed_two_sessions stamps no projects (the
    /// active key would pass both otherwise).
    #[test]
    fn prompts_for_a_background_session_name_that_session() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        if let Some(bucket) = app.sessions.get_mut(&key_a) {
            bucket.project = "alpha".to_owned();
        }
        if let Some(bucket) = app.sessions.get_mut(&key_b) {
            bucket.project = "beta".to_owned();
        }
        app.active_session_key = Some(key_a.clone());
        if let Some(ws) = app.workspace.as_ref() {
            ws.insert_live_worker(
                &forge_workspace::ProjectKey::new("p-beta"),
                forge_workspace::WorkerEntry {
                    label: "egen-lead".to_owned(),
                    charter: String::new(),
                    slot: key_b.clone(),
                    session_id: None,
                    status: forge_primitives::WorkerLiveness::Running,
                    spawned_at: std::time::SystemTime::UNIX_EPOCH,
                    spawned_by: SessionSlot::from_str_for_test(""),
                    needs_tag: false,
                    is_git_repo_at_spawn: false,
                    diagnostic: None,
                    kick: None,
                },
            );
        }
        let context = crate::app::notify::NotifyContext {
            project: "beta".to_owned(),
            worker_label: Some("egen-lead".to_owned()),
        };
        apply_session_update(
            &mut app,
            SessionUpdate::QuestionRequest {
                key: key_b.clone(),
                tool_id: "tc-q-bg".into(),
                request: crate::app::prompt::tests::make_question_request(false),
            },
        );
        apply_session_update(
            &mut app,
            SessionUpdate::PermissionRequest {
                key: key_b,
                tool_id: "tc-p-bg".into(),
                request: crate::app::prompt::tests::make_permission_request(),
            },
        );
        assert_eq!(
            crate::app::notify::test_capture::take_notifications(&app),
            vec![
                (crate::app::notify::NotifyEvent::QuestionRequired, context.clone()),
                (crate::app::notify::NotifyEvent::PermissionRequired, context),
            ],
            "both prompt kinds name the background session's project + worker",
        );
    }

    /// A prompt for a session the TUI has no bucket for is dropped
    /// rather than queued, so there is nothing for the user to answer.
    /// Notifying anyway would send them looking for a prompt that does
    /// not exist.
    #[test]
    fn a_dropped_prompt_raises_no_notification() {
        let mut app = App::test_default();
        let (_key_a, _key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            SessionUpdate::QuestionRequest {
                key: forge_workspace::SessionSlot::from_str_for_test("no-such-session"),
                tool_id: "tc-orphan".into(),
                request: crate::app::prompt::tests::make_question_request(false),
            },
        );
        assert!(
            crate::app::notify::test_capture::take_notifications(&app).is_empty(),
            "a prompt that never reached a queue must not claim the user's attention",
        );
    }

    /// Background-session permission requests must enqueue onto the
    /// target bucket's queue, not the active bucket's queue. Same
    /// routing rule as the rest of the multiplexer.
    #[test]
    fn permission_request_event_enqueues_on_background_target_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        let request = crate::app::prompt::tests::make_permission_request();
        apply_session_update(
            &mut app,
            SessionUpdate::PermissionRequest {
                key: key_b.clone(),
                tool_id: "tc-bg".into(),
                request,
            },
        );
        assert_eq!(
            app.sessions.get(&key_b).expect("b").prompt_queue.len(),
            1,
            "background target enqueues onto its own queue"
        );
        assert!(
            app.sessions.get(&key_a).expect("a").prompt_queue.is_empty(),
            "active session must not receive a background bucket's prompt"
        );
    }

    /// Helper: build a `WorkerStatusChanged { Removed }` event with
    /// the given label + worktree disposition for the worker-close-toast
    /// tests.
    fn worker_removed_event(
        label: &str,
        worktree: forge_workspace::protocol::WorktreeDisposition,
    ) -> SessionUpdate {
        SessionUpdate::WorkerStatusChanged {
            project_key: forge_workspace::ProjectKey::new("forge"),
            action: forge_workspace::protocol::WorkerStatusAction::Removed,
            status: forge_primitives::WorkerStatus {
                label: label.to_owned(),
                charter: "test".to_owned(),
                status: forge_primitives::WorkerLiveness::Running,
                session_id: "uuid-1".to_owned(),
                slot: SessionSlot::from_str_for_test("uuid-1"),
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
                diagnostic: None,
                activity: None,
            },
            worktree,
        }
    }

    /// Concatenate every text span of a chat-message body so test
    /// assertions can scan the full rendered text.
    fn chat_message_text(msg: &crate::app::ChatMessage) -> String {
        msg.blocks
            .iter()
            .map(|b| match b {
                crate::app::MessageBlock::Text(t) => t.text.clone(),
                _ => String::new(),
            })
            .collect::<String>()
    }

    #[test]
    fn worker_removed_event_pushes_close_toast_for_git_repo_worker() {
        let mut app = App::test_default();
        let lead_key = SessionSlot::from_str_for_test("lead-uuid");
        app.sessions.insert(
            lead_key.clone(),
            crate::app::session::UiSession::new(lead_key.clone(), "test-project"),
        );
        let before = app.sessions.get(&lead_key).expect("lead bucket").messages.len();

        apply_session_update(
            &mut app,
            worker_removed_event("reviewer", WorktreeDisposition::Intact),
        );

        let bucket = app.sessions.get(&lead_key).expect("lead bucket");
        assert!(bucket.messages.len() > before, "Removed event should push a system-message toast");
        let toast = chat_message_text(bucket.messages.last().expect("toast message present"));
        assert!(toast.contains("Worker reviewer closed."), "missing close text: {toast:?}");
        assert!(
            toast.contains("Worktree preserved at .claude/worktrees/reviewer/"),
            "missing worktree-preserved text: {toast:?}",
        );
        assert!(app.needs_redraw, "Removed event must request a redraw");
    }

    #[test]
    fn worker_removed_event_pushes_plain_toast_for_an_absent_worktree() {
        let mut app = App::test_default();
        let lead_key = SessionSlot::from_str_for_test("lead-uuid");
        app.sessions.insert(
            lead_key.clone(),
            crate::app::session::UiSession::new(lead_key.clone(), "test-project"),
        );

        apply_session_update(&mut app, worker_removed_event("notes", WorktreeDisposition::Absent));

        let bucket = app.sessions.get(&lead_key).expect("lead bucket");
        let toast = chat_message_text(bucket.messages.last().expect("toast message present"));
        assert_eq!(toast, "Worker notes closed.");
        assert!(!toast.contains("worktree"), "must not mention worktree: {toast:?}");
        assert!(!toast.contains("Worktree"), "must not mention worktree: {toast:?}");
    }

    /// The close toast must land in the worker's OWNING lead session
    /// (its `spawned_by_session_id`), never the focused one. Viewing a
    /// different project while a worker despawns must not leak the
    /// toast into that project's chat.
    #[test]
    fn worker_removed_event_routes_close_toast_to_lead_not_active() {
        let mut app = App::test_default();
        let lead_key = SessionSlot::from_str_for_test("lead-uuid");
        let focused_key = SessionSlot::from_str_for_test("other-uuid");
        app.sessions.insert(
            lead_key.clone(),
            crate::app::session::UiSession::new(lead_key.clone(), "test-project"),
        );
        app.sessions.insert(
            focused_key.clone(),
            crate::app::session::UiSession::new(focused_key.clone(), "test-project"),
        );
        app.active_session_key = Some(focused_key.clone());
        let lead_before = app.sessions.get(&lead_key).expect("lead bucket").messages.len();
        let focused_before = app.sessions.get(&focused_key).expect("focused bucket").messages.len();

        apply_session_update(
            &mut app,
            worker_removed_event("reviewer", WorktreeDisposition::Absent),
        );

        let lead = app.sessions.get(&lead_key).expect("lead bucket");
        assert!(
            lead.messages.len() > lead_before,
            "toast must land in the worker's owning lead session",
        );
        let toast = chat_message_text(lead.messages.last().expect("toast message present"));
        assert_eq!(toast, "Worker reviewer closed.");
        assert_eq!(
            app.sessions.get(&focused_key).expect("focused bucket").messages.len(),
            focused_before,
            "the focused session must not receive the worker-close toast",
        );
    }

    /// When the worker's lead session isn't live in this process (its
    /// project isn't open here), the toast is dropped rather than
    /// leaked into the focused session.
    #[test]
    fn worker_removed_event_drops_close_toast_when_lead_not_live() {
        let mut app = App::test_default();
        let active_key =
            app.active_session_key.clone().expect("test_default seeds an active session");
        // No `lead-uuid` bucket: the worker's owning project isn't open here.
        let active_before = app.sessions.get(&active_key).expect("active bucket").messages.len();

        apply_session_update(
            &mut app,
            worker_removed_event("reviewer", WorktreeDisposition::Intact),
        );

        assert_eq!(
            app.sessions.get(&active_key).expect("active bucket").messages.len(),
            active_before,
            "no toast may leak to the focused session when the lead isn't live",
        );
    }

    /// Removed must drop the worker's UiSession bucket. Without
    /// this the stale bucket lingers in `app.sessions` and a later
    /// `active_session_key == worker_key` render keeps showing the
    /// closed worker's chat history.
    #[test]
    fn worker_removed_event_drops_worker_session_bucket() {
        let mut app = App::test_default();
        let worker_key = SessionSlot::from_str_for_test("uuid-1");
        app.sessions.insert(
            worker_key.clone(),
            crate::app::session::UiSession::new(worker_key.clone(), "test-project"),
        );
        assert!(app.sessions.contains_key(&worker_key));

        apply_session_update(
            &mut app,
            worker_removed_event("reviewer", WorktreeDisposition::Intact),
        );

        assert!(
            !app.sessions.contains_key(&worker_key),
            "worker bucket must be dropped from app.sessions on Removed"
        );
    }

    /// When `active_session_key` was the closed worker, Removed must
    /// fall back to the worker's spawning lead (the natural revert).
    #[test]
    fn worker_removed_event_falls_back_active_to_lead_when_worker_was_active() {
        let mut app = App::test_default();
        let lead_key = SessionSlot::from_str_for_test("lead-uuid");
        let worker_key = SessionSlot::from_str_for_test("uuid-1");
        app.sessions.insert(
            lead_key.clone(),
            crate::app::session::UiSession::new(lead_key.clone(), "test-project"),
        );
        app.sessions.insert(
            worker_key.clone(),
            crate::app::session::UiSession::new(worker_key.clone(), "test-project"),
        );
        app.active_session_key = Some(worker_key.clone());

        apply_session_update(
            &mut app,
            worker_removed_event("reviewer", WorktreeDisposition::Intact),
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&lead_key),
            "active session must fall back to the spawning lead",
        );
    }

    /// When the worker's lead isn't in `app.sessions` either (rare:
    /// the lead's bucket was already torn down), fall back to any
    /// surviving session rather than dropping active to None.
    #[test]
    fn worker_removed_event_falls_back_to_any_session_when_lead_gone() {
        let mut app = App::test_default();
        let worker_key = SessionSlot::from_str_for_test("uuid-1");
        // Note: no lead-uuid bucket present. The test_default()
        // helper already seeded an active session under
        // `App::TEST_SESSION_KEY`; that bucket plays the role of
        // "any surviving session" for this test.
        app.sessions.insert(
            worker_key.clone(),
            crate::app::session::UiSession::new(worker_key.clone(), "test-project"),
        );
        app.active_session_key = Some(worker_key.clone());

        apply_session_update(
            &mut app,
            worker_removed_event("reviewer", WorktreeDisposition::Intact),
        );

        assert_ne!(
            app.active_session_key.as_ref(),
            Some(&worker_key),
            "active must no longer point at the removed worker"
        );
        assert!(
            app.active_session_key.is_some(),
            "active must fall back to a surviving session, not None, while any bucket exists",
        );
        assert!(!app.sessions.contains_key(&worker_key), "worker bucket itself must be gone");
    }

    /// Seed projects `alpha` and `bravo`, each with a lead bucket,
    /// plus `workers` under alpha in spawn order. A worker flagged
    /// `false` gets its `live_workers` entry but no `app.sessions`
    /// bucket - the spawning window before its Connected lands.
    fn seed_projects_with_workers(app: &mut App, workers: &[(&str, bool)]) {
        let ws = app.workspace.clone().expect("test workspace");
        for project in ["bravo", "alpha"] {
            ws.seed_test_project(project, &format!("/tmp/{project}"));
            let lead = SessionSlot::from_str_for_test(format!("{project}-lead"));
            let mut bucket = UiSession::new(lead.clone(), project);
            bucket.cwd_raw = format!("/tmp/{project}");
            app.sessions.insert(lead, bucket);
        }
        let alpha =
            ws.list_projects().into_iter().find(|p| p.name == "alpha").expect("seeded project").key;
        for (label, has_bucket) in workers {
            let key = SessionSlot::from_str_for_test(*label);
            if *has_bucket {
                app.sessions.insert(key.clone(), UiSession::new(key.clone(), "alpha"));
            }
            ws.insert_live_worker(
                &alpha,
                forge_workspace::WorkerEntry {
                    label: (*label).to_owned(),
                    charter: "charter".to_owned(),
                    slot: key,
                    session_id: None,
                    status: forge_primitives::WorkerLiveness::Running,
                    spawned_at: std::time::SystemTime::UNIX_EPOCH,
                    spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
                    needs_tag: false,
                    is_git_repo_at_spawn: false,
                    diagnostic: None,
                    kick: None,
                },
            );
        }
    }

    /// A Removed event for the worker whose slot is `label`, spawned by
    /// a lead that names no live bucket - so the lead preference cannot
    /// fire and the fallback is what is on test.
    fn worker_removed_orphaned(label: &str) -> SessionUpdate {
        let mut event = worker_removed_event(label, WorktreeDisposition::Absent);
        if let SessionUpdate::WorkerStatusChanged { status, .. } = &mut event {
            status.session_id = label.to_owned();
            status.slot = SessionSlot::from_str_for_test(label);
        }
        event
    }

    /// With the spawning lead gone, Removed lands on the row the
    /// Projects pane draws under the closed worker: the next worker
    /// in the project's subtree, and the following project's lead
    /// once the closed worker was the last one. Each position is
    /// removed from its own fresh App, so a pick out of the
    /// `app.sessions` HashMap would have to guess all three.
    #[test]
    fn worker_removed_event_lands_on_the_adjacent_drawn_row_when_lead_gone() {
        // Drawn: alpha's lead, its workers in spawn order, then bravo's.
        let workers = [("w-one", true), ("w-two", true), ("w-three", true)];
        let cases = [("w-one", "w-two"), ("w-two", "w-three"), ("w-three", "bravo-lead")];

        for (closing, expected) in cases {
            let mut app = App::test_default();
            seed_projects_with_workers(&mut app, &workers);
            app.active_session_key = Some(SessionSlot::from_str_for_test(closing));

            apply_session_update(&mut app, worker_removed_orphaned(closing));

            assert_eq!(
                app.active_session_key.as_ref().map(|k| k.label().to_owned()),
                Some(expected.to_owned()),
                "removing {closing} must land on the row drawn under it",
            );
        }
    }

    /// A worker still inside its spawning window draws a row but has
    /// no bucket yet - the same window `switch_to_worker` refuses a
    /// click in. Focus passes over that row: nominating it would put
    /// the switch through a key `app.sessions` cannot resolve, which
    /// `switch_active_session` declines, leaving active pointed at
    /// the worker that was just removed.
    #[test]
    fn worker_removed_event_skips_a_drawn_row_whose_bucket_has_not_landed() {
        let mut app = App::test_default();
        seed_projects_with_workers(
            &mut app,
            &[("w-one", true), ("w-two", false), ("w-three", true)],
        );
        app.active_session_key = Some(SessionSlot::from_str_for_test("w-one"));

        apply_session_update(&mut app, worker_removed_orphaned("w-one"));

        assert_eq!(
            app.active_session_key.as_ref().map(|k| k.label().to_owned()),
            Some("w-three".to_owned()),
            "the spawning worker's row has no bucket to focus, so the pick moves past it",
        );
    }

    /// When `active_session_key` was NOT the closed worker, it must
    /// stay put after Removed. Closing a worker while viewing the
    /// lead's chat must not yank the user away from the lead.
    #[test]
    fn worker_removed_event_leaves_active_unchanged_when_not_active() {
        let mut app = App::test_default();
        let lead_key = SessionSlot::from_str_for_test("lead-uuid");
        let worker_key = SessionSlot::from_str_for_test("uuid-1");
        app.sessions.insert(
            lead_key.clone(),
            crate::app::session::UiSession::new(lead_key.clone(), "test-project"),
        );
        app.sessions.insert(
            worker_key.clone(),
            crate::app::session::UiSession::new(worker_key.clone(), "test-project"),
        );
        app.active_session_key = Some(lead_key.clone());

        apply_session_update(
            &mut app,
            worker_removed_event("reviewer", WorktreeDisposition::Intact),
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&lead_key),
            "active session must stay on the lead when the closed worker wasn't active",
        );
        assert!(!app.sessions.contains_key(&worker_key));
    }

    fn review_notice(key: &SessionSlot, waiting: usize) -> SessionUpdate {
        SessionUpdate::ReviewActivityNotice {
            key: key.clone(),
            branch: "feat".to_owned(),
            waiting,
            message: "worker addressed review #1".to_owned(),
        }
    }

    /// The notice's tally scrolls away with the chat, so the count it
    /// carries is parked on the target bucket where both the GIT badge
    /// and the attention band can read it every frame.
    #[test]
    fn review_activity_notice_parks_the_waiting_count_on_the_target_bucket() {
        let mut app = App::test_default();
        let (_key_a, key_b) = seed_two_sessions(&mut app);

        apply_session_update(&mut app, review_notice(&key_b, 2));

        let waiting = app
            .sessions
            .get(&key_b)
            .and_then(|s| s.review_replies_waiting.clone())
            .expect("the notice parks a waiting signal");
        assert_eq!(waiting.count, 2);
        assert_eq!(waiting.branch, "feat", "scoped to the branch the review is on");
    }

    /// A later notice reporting nothing left to read (the reviewer
    /// answered from the worker's side, or resolved elsewhere) clears
    /// the signal rather than leaving a stale badge lit.
    #[test]
    fn review_activity_notice_with_nothing_waiting_clears_the_signal() {
        let mut app = App::test_default();
        let (_key_a, key_b) = seed_two_sessions(&mut app);

        apply_session_update(&mut app, review_notice(&key_b, 2));

        // Another branch reporting nothing waiting says nothing about
        // `feat` - only a reviewer turn on `feat` retires those answers.
        apply_session_update(
            &mut app,
            SessionUpdate::ReviewActivityNotice {
                key: key_b.clone(),
                branch: "other".to_owned(),
                waiting: 0,
                message: "worker addressed review #2".to_owned(),
            },
        );
        assert_eq!(
            app.sessions
                .get(&key_b)
                .and_then(|s| s.review_replies_waiting.clone())
                .map(|w| w.count),
            Some(2),
            "a zero on another branch leaves the live count alone",
        );

        apply_session_update(&mut app, review_notice(&key_b, 0));
        assert!(
            app.sessions.get(&key_b).and_then(|s| s.review_replies_waiting.clone()).is_none(),
            "a zero count clears the parked signal",
        );
    }
}

/// The spawn-stub focus seam. An id-less focused bucket - a
/// `Spawning` stub - must not inherit a background session's
/// identity, or `set_session_id` drags focus there and the spawn's
/// own Connected finds the stub unfocused. A frame is addressed to
/// the slot its producer stated, so a background frame routes to its
/// own bucket and a frame for a slot this process does not hold is
/// dropped; neither can reach the stub.
#[cfg(test)]
mod focus_seam_tests {
    use super::*;
    use crate::app::MessageBlock;
    use crate::app::session::UiSession;

    fn user_frame(session_id: &str) -> forge_primitives::Message {
        forge_primitives::Message::User {
            message: forge_primitives::UserEnvelope {
                role: "user".to_owned(),
                content: vec![forge_primitives::ContentBlock::Text {
                    text: "background turn".to_owned(),
                }],
            },
            session_id: session_id.to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    fn assistant_frame(session_id: &str) -> forge_primitives::Message {
        forge_primitives::Message::Assistant {
            message: forge_primitives::AssistantEnvelope {
                id: "msg_1".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-opus-5".to_owned(),
                content: vec![forge_primitives::ContentBlock::Text {
                    text: "background answer".to_owned(),
                }],
                stop_reason: None,
                stop_sequence: None,
                usage: None,
            },
            session_id: session_id.to_owned(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
        }
    }

    /// A live worker bucket plus the user sitting on a cold project's
    /// spawn stub.
    fn app_with_worker_and_focused_stub() -> (App, SessionSlot, SessionSlot) {
        let mut app = App::test_default();
        let worker = SessionSlot::from_str_for_test("worker-uuid");
        let mut worker_bucket = UiSession::new(worker.clone(), "test-project");
        worker_bucket.session_id = Some(forge_primitives::SessionId::new("worker-uuid"));
        app.sessions.insert(worker.clone(), worker_bucket);
        let stub = SessionSlot::from_str_for_test("busymail-session-uuid");
        app.sessions.insert(stub.clone(), UiSession::new(stub.clone(), "busymail"));
        app.active_session_key = Some(stub.clone());
        (app, stub, worker)
    }

    /// A frame from a bucketed background session must route to its
    /// own bucket without touching the stub - adopting its session id
    /// onto the id-less stub dragged focus to the worker and the user
    /// landed there when the spawn connected.
    #[test]
    fn foreign_frame_spares_the_idless_spawn_stub() {
        let (mut app, stub, worker) = app_with_worker_and_focused_stub();

        apply_session_update_chat_appended(
            &mut app,
            &SessionSlot::from_str_for_test("worker-uuid"),
            user_frame("worker-uuid"),
        );
        apply_session_update_chat_appended(
            &mut app,
            &SessionSlot::from_str_for_test("worker-uuid"),
            assistant_frame("worker-uuid"),
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&stub),
            "a frame from a bucketed session must not drag focus off the stub",
        );
        assert!(
            app.sessions.get(&stub).and_then(|b| b.session_id.clone()).is_none(),
            "the background session's id must not be stamped onto the stub",
        );
        // The frame's content must land in its own bucket's chat, not
        // in the focused stub's.
        let worker_bucket = app.sessions.get(&worker).expect("worker bucket");
        assert!(
            worker_bucket.messages.iter().any(|m| m.blocks.iter().any(|b| matches!(
                b, MessageBlock::Text(t) if t.text.contains("background answer")
            ))),
            "the background frame renders into its own session's chat",
        );
        assert!(
            !app.sessions.get(&stub).expect("stub bucket").messages.iter().any(|m| m
                .blocks
                .iter()
                .any(
                    |b| matches!(b, MessageBlock::Text(t) if t.text.contains("background answer"))
                )),
            "the background frame must not render into the focused stub's chat",
        );
    }

    /// End to end: with the frame gated, the clicked session keeps
    /// focus through its own Connected, which arrives under the id the
    /// spawn announced - so the click lands on the connecting session
    /// without a re-click.
    #[test]
    fn click_spawn_keeps_focus_through_connect() {
        let (mut app, stub, _worker) = app_with_worker_and_focused_stub();
        *app.resuming_session_id_mut().expect("active session") = Some("resume-1".to_owned());

        apply_session_update_chat_appended(
            &mut app,
            &SessionSlot::from_str_for_test("worker-uuid"),
            user_frame("worker-uuid"),
        );
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Connected {
                key: stub.clone(),
                session_id: forge_primitives::SessionId::new(stub.display()),
                cwd: "/Users/vedhavyas/Projects/busymail".to_owned(),
                current_model: forge_primitives::CurrentModel::new("claude-opus-5", "opus", "Opus"),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&stub),
            "the clicked session keeps focus through its Connected",
        );
        assert_eq!(
            app.resuming_session_id(),
            None,
            "Connected took the active path: the user is watching this session",
        );
    }

    /// A session the user is already watching must never be taken by
    /// a background auto_start wake: `App::test_default` seeds the
    /// one focused session, and every auto_start session connects
    /// behind it. None of them may take the tab - not at the stub,
    /// not at connect, and not via a stray frame landing on the
    /// id-less stub.
    #[test]
    fn background_boot_connects_never_take_focus() {
        let mut app = App::test_default();
        let conn_pending = SessionSlot::from_str_for_test(crate::app::App::TEST_SESSION_KEY);
        // One key for the whole wake: the spawn announces the id the
        // session will connect under, so there is no stub to carry.
        let real = SessionSlot::from_str_for_test("bg-uuid");
        let stub = real.clone();

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: stub.clone(),
                project_name: "forge".to_owned(),
                cwd: "/Users/vedhavyas/Projects/forge".to_owned(),
                display_name: "forge".to_owned(),
            },
        );
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&conn_pending),
            "a background wake's stub registers without taking focus",
        );

        apply_session_update_chat_appended(
            &mut app,
            &SessionSlot::from_str_for_test("bg-uuid"),
            user_frame("bg-uuid"),
        );
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Connected {
                key: real.clone(),
                session_id: forge_primitives::SessionId::new("bg-uuid"),
                cwd: "/Users/vedhavyas/Projects/forge".to_owned(),
                current_model: forge_primitives::CurrentModel::new("claude-opus-5", "opus", "Opus"),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                compaction_count: 0,
            },
        );

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&conn_pending),
            "a background wake's connect must never move focus",
        );
    }
}
