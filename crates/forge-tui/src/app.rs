pub(crate) mod active_bucket_scope;
mod cache_policy;
pub(crate) mod cli_version;
pub(crate) mod clipboard_image;
pub(crate) mod config;
pub(crate) mod connect;
mod dialog;
pub(crate) mod diff_overlay;
pub(crate) mod events;
pub(crate) mod file_index;
mod focus;
pub(crate) mod git_diff;
mod inline_interactions;
pub(crate) mod input;
mod input_submit;
mod keys;
pub(crate) mod launchpad;
pub(crate) mod mention;
mod notify;
pub(crate) mod paste_burst;
mod permissions;
pub(crate) mod plugins;
pub(crate) mod process_scanner;
pub(crate) mod processes;
mod questions;
mod selection;
mod service_status_check;
pub mod session;
mod session_runtime;
pub(crate) mod slash;
mod state;
pub(crate) mod subagent;
mod tab_title;
mod terminal;
mod todos;
pub(crate) mod usage;
pub(crate) mod view;

// Re-export all public types so `crate::app::App`, `crate::app::BlockCache`, etc. still work.
pub use cache_policy::{
    CacheSplitPolicy, DEFAULT_CACHE_SPLIT_HARD_LIMIT_BYTES, DEFAULT_CACHE_SPLIT_SOFT_LIMIT_BYTES,
    DEFAULT_TOOL_PREVIEW_LIMIT_BYTES, TextSplitDecision, TextSplitKind, default_cache_split_policy,
    find_text_split, find_text_split_index,
};
pub use config::ConfigState;
pub use connect::{create_app, start_connection};
pub use diff_overlay::DiffOverlayState;
pub use events::{apply_session_update, handle_terminal_event};
#[cfg(feature = "testing")]
pub use events::{handle_permission_request_event, handle_question_request_event};
pub use focus::{FocusManager, FocusOwner, FocusTarget};
pub use input::InputState;
pub use launchpad::LaunchpadState;
pub(crate) use selection::normalize_selection;
pub use service_status_check::start_service_status_check;
pub(crate) use state::MarkdownRenderKey;
pub(crate) use state::cache_metrics;
pub use state::{
    App, AppStatus, BlockCache, CacheMetrics, CachedMessageSegment, ChatMessage,
    ChatRenderTraceState, ChatViewport, ExtraUsage, HelpView, IncrementalMarkdown,
    InlinePermission, InlineQuestion, InvalidationLevel, LayoutInvalidation, LoginHint, McpState,
    MessageBlock, MessageRenderCache, MessageRenderCacheKey, MessageRenderSignature, MessageRole,
    MessageUsage, ModeInfo, ModeState, NoticeBlock, NoticeDedupKey, NoticeStage, PaneHitTarget,
    PasteSessionState, PendingCommandAck, RateLimitIncidentKey, RecentSessionInfo,
    ScrollbarGeometry, SelectionKind, SelectionPoint, SelectionState, SessionTurnState,
    SessionUsageState, SystemSeverity, TerminalSnapshotMode, TextBlock,
    TextBlockSpacing, TodoItem, TodoStatus, ToolCallInfo, ToolCallScope, TurnNoticeLocation,
    TurnNoticeRef, UsageSnapshot, UsageSourceKind, UsageSourceMode, UsageState, UsageWindow,
    WelcomeBlock, compute_scrollbar_geometry, hash_text_block_content, hash_welcome_block_content,
    is_execute_tool_name,
};
pub use view::ActiveView;

use crossterm::event::{
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use futures::{FutureExt as _, StreamExt};
use std::time::{Duration, Instant};

const SPINNER_FRAME_INTERVAL_NORMAL: Duration = Duration::from_millis(30);
const SPINNER_FRAME_INTERVAL_REDUCED: Duration = Duration::from_millis(120);

/// Hard cap on candidates shown in autocomplete dropdowns
/// (file_index, slash, subagent). Same value across the three so a
/// single change here covers every dropdown.
pub(crate) const MAX_CANDIDATES: usize = 50;

// ---------------------------------------------------------------------------
// Terminal suspend / resume helpers (reused by /login, /logout)
// ---------------------------------------------------------------------------

/// Disable raw mode and crossterm features so a child process can own the
/// terminal (e.g. `claude auth login` which opens a browser flow).
pub(crate) fn suspend_terminal() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableFocusChange,
        PopKeyboardEnhancementFlags
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Re-enable raw mode and crossterm features after a child process finishes.
pub(crate) fn resume_terminal() {
    let _ = crossterm::terminal::enable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableFocusChange,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
}

// ---------------------------------------------------------------------------
// TUI event loop
// ---------------------------------------------------------------------------

pub async fn run_tui(app: &mut App) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let mut os_shutdown = Box::pin(wait_for_shutdown_signal());

    // Enable bracketed paste, mouse capture, and enhanced keyboard protocol
    resume_terminal();

    let mut events = EventStream::new();
    // 4ms tick → ~250 Hz nominal sleep ceiling; practical FPS during
    // animation tops out around 120-150 once render overhead (~3ms
    // per frame) is included. Idle state costs nothing because the
    // render loop skips when `needs_redraw == false`; only the
    // wakeup cadence increases (negligible on modern hardware).
    let tick_duration = Duration::from_millis(4);
    let mut last_render = Instant::now();

    loop {
        start_connection(app);

        // Wait for an event or the next frame tick.
        let time_to_next = tick_duration.saturating_sub(last_render.elapsed());
        tokio::select! {
            Some(Ok(event)) = events.next() => {
                events::handle_terminal_event(app, event);
            }
            Some(update) = app.update_rx.recv() => {
                events::apply_session_update(app, update);
            }
            shutdown = &mut os_shutdown => {
                if let Err(err) = shutdown {
                    tracing::warn!(
                        target: crate::logging::targets::APP_LIFECYCLE,
                        event_name = "os_shutdown_listener_failed",
                        message = "OS shutdown signal listener failed",
                        outcome = "failure",
                        error_message = %err,
                    );
                }
                app.should_quit = true;
            }
            () = tokio::time::sleep(time_to_next) => {}
        }

        // Drain any remaining queued events without blocking.
        loop {
            // Try terminal events first (keeps typing responsive)
            if let Some(Some(Ok(event))) = events.next().now_or_never() {
                events::handle_terminal_event(app, event);
                continue;
            }
            // Then SessionUpdates from workspace
            match app.update_rx.try_recv() {
                Ok(update) => {
                    events::apply_session_update(app, update);
                }
                Err(_) => break,
            }
        }

        file_index::drain_events(app);
        git_diff::drain_events(app);
        cli_version::drain_events(app);
        process_scanner::drain_events(app);
        diff_overlay::drain_events(app);

        // If a prior turn ended in Error state because of a rate-limit
        // rejection, drop the input lock once the rate-limit window
        // has passed. The CLI doesn't proactively emit RateLimitEvent
        // updates without a fresh request, so we poll the wall clock
        // each tick instead of relying on the wire.
        events::rate_limit::maybe_recover_from_rate_limit_lock(app);

        // The Projects pane's account/status panel renders 5h + 7d
        // usage bars on every frame. Keep the snapshot live by
        // calling `request_refresh_if_needed` each tick — it's
        // idempotent (no-ops while the existing snapshot is younger
        // than `USAGE_REFRESH_TTL` and while a request is in flight),
        // so the actual fetch only fires once per TTL window.
        crate::app::usage::request_refresh_if_needed(app);

        // Tick the burst detector: flush any held/buffered content
        // that has timed out. EmitChar re-inserts a single held
        // character; EmitPaste feeds the accumulated burst into the
        // paste queue.
        if app.active_view == ActiveView::Chat
            && let Some(action) = app.paste_burst.tick(Instant::now())
        {
            match action {
                paste_burst::FlushAction::EmitChar(ch) => {
                    let _ = app.input_mut().textarea_insert_char(ch);
                }
                paste_burst::FlushAction::EmitPaste(text) => {
                    app.queue_paste_text(&text);
                }
            }
        }

        // Merge and process `Event::Paste` chunks as one paste action.
        if app.active_view == ActiveView::Chat && !app.pending_paste_text().is_empty() {
            finalize_pending_paste_event(app);
        }

        // Deferred submit: if Enter was pressed and no paste payload arrived
        // in this drain cycle, restore the exact pre-submit snapshot and
        // submit that unchanged draft.
        if app.active_view == ActiveView::Chat && app.pending_submit().is_some() {
            finalize_deferred_submit(app);
        }

        if app.should_quit {
            break;
        }

        // Render once, only when something changed. The extra
        // is_animating clause keeps the per-row spinners on background
        // Running / Spawning sessions animating; the active session
        // already drives ticks via `app.status` above.
        let any_background_running = app.sessions.values().any(|s| {
            matches!(
                s.lifecycle_state,
                crate::app::session::SessionLifecycleState::Running
                    | crate::app::session::SessionLifecycleState::Spawning
            )
        });
        let is_animating = matches!(
            app.status,
            AppStatus::Connecting
                | AppStatus::CommandPending
                | AppStatus::Thinking
                | AppStatus::Running
        ) || app.is_compacting()
            || any_background_running;
        if is_animating {
            advance_spinner_frame(app, Instant::now());
            tab_title::update_tab_title(&app.status, app.spinner_frame, app.cwd());
            app.needs_redraw = true;
        } else {
            app.spinner_last_advance_at = None;
        }
        // Update tab title on non-animating state transitions (Ready, Error).
        if !is_animating && app.needs_redraw {
            tab_title::update_tab_title(&app.status, app.spinner_frame, app.cwd());
        }
        // Smooth scroll still settling — viewport row index (usize)
        // converts to f32 for sub-pixel scroll comparison; loss is bounded
        // by terminal height so precision is irrelevant here.
        #[allow(clippy::cast_precision_loss)]
        let scroll_delta = (app.viewport().scroll_target as f32 - app.viewport().scroll_pos).abs();
        if scroll_delta >= 0.01 {
            app.needs_redraw = true;
        }
        if terminal::update_terminal_outputs(app) {
            app.needs_redraw = true;
        }
        if app.force_redraw {
            terminal.clear()?;
            app.force_redraw = false;
            app.needs_redraw = true;
        }
        if app.needs_redraw {
            if let Some(ref mut perf) = app.perf {
                perf.next_frame();
            }
            // FPS overlay is always-on (see
            // `chat_view::render_perf_fps_overlay`), not gated on the `perf`
            // Cargo feature. `mark_frame_presented` keeps the EMA fresh so the
            // overlay shows real numbers in any build.
            app.mark_frame_presented(Instant::now());
            // `Timer` is `Drop`-implementing under `feature = "perf"` and a
            // unit struct otherwise. Explicit `drop()` enforces the desired
            // lifetime in both feature paths; clippy can't see the cfg
            // branch where Drop matters.
            #[allow(clippy::drop_non_drop)]
            {
                let timer = app.perf.as_ref().map(|p| p.start("frame_total"));
                let draw_timer = app.perf.as_ref().map(|p| p.start("frame::terminal_draw"));
                terminal.draw(|f| crate::ui::render(f, app))?;
                drop(draw_timer);
                drop(timer);
            }
            app.needs_redraw = false;
            last_render = Instant::now();
        }
    }

    // --- Graceful shutdown ---

    // Dismiss all pending inline permissions / questions (reject via
    // last option / cancelled). Outcomes route through
    // `Workspace::dispatch` — the workspace-side `SessionTask` pops
    // the matching slot and the bridge forwards it to the agent.
    let active_session_key = app.active_session_key.clone();
    for tool_id in std::mem::take(app.pending_interaction_ids_mut()) {
        let (perm_last_option_id, question_was_pending) = if let Some((mi, bi)) =
            app.lookup_tool_call(&tool_id)
            && let Some(MessageBlock::ToolCall(tc)) =
                app.active_messages_mut().get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
        {
            let tc = tc.as_mut();
            let perm_last = tc
                .pending_permission
                .take()
                .and_then(|p| p.options.last().map(|opt| opt.option_id.clone()));
            let question_taken = tc.pending_question.take().is_some();
            (perm_last, question_taken)
        } else {
            (None, false)
        };
        let Some(session_key) = active_session_key.as_ref() else {
            continue;
        };
        if let Some(option_id) = perm_last_option_id {
            crate::app::events::turn::dispatch_permission_outcome(
                app,
                session_key,
                &tool_id,
                forge_primitives::PermissionOutcome::Selected { option_id },
            );
        }
        if question_was_pending {
            crate::app::events::turn::dispatch_question_outcome(
                app,
                session_key,
                &tool_id,
                forge_primitives::QuestionOutcome::Cancelled,
            );
        }
    }

    // Cancel any active turn and give the adapter a moment to clean up
    if matches!(app.status, AppStatus::Thinking | AppStatus::Running)
        && app.has_active_agent()
        && app.session_id().is_some()
    {
        let _ = app.dispatch_command(|key| forge_workspace::Command::Cancel { key });
    }

    // Restore terminal
    tab_title::restore_tab_title(app.cwd());
    suspend_terminal();
    ratatui::restore();

    Ok(())
}

fn advance_spinner_frame(app: &mut App, now: Instant) {
    let interval = if app.config.prefers_reduced_motion_effective() {
        SPINNER_FRAME_INTERVAL_REDUCED
    } else {
        SPINNER_FRAME_INTERVAL_NORMAL
    };

    match app.spinner_last_advance_at {
        Some(last_advance) if now.duration_since(last_advance) < interval => {}
        Some(_) | None => {
            app.spinner_frame = app.spinner_frame.wrapping_add(1);
            app.spinner_last_advance_at = Some(now);
        }
    }
}

async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            sigint = tokio::signal::ctrl_c() => {
                sigint?;
            }
            _ = sigterm.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Finalize queued `Event::Paste` chunks for this drain cycle.
fn finalize_pending_paste_event(app: &mut App) {
    let pasted = std::mem::take(app.pending_paste_text_mut());
    if pasted.is_empty() {
        return;
    }
    let pasted_chars = pasted.chars().count();

    let session = app.pending_paste_session_mut().take().unwrap_or_else(|| {
        let id = app.allocate_paste_session_id();
        state::PasteSessionState {
            id,
            start: SelectionPoint { row: app.input().cursor_row(), col: app.input().cursor_col() },
            placeholder_index: None,
        }
    });
    let session_id = session.id;

    if session.placeholder_index.is_none() {
        let end = SelectionPoint { row: app.input().cursor_row(), col: app.input().cursor_col() };
        strip_input_range(app, session.start, end);
    }

    let appended = session
        .placeholder_index
        .and_then(|session_idx| {
            let current_line = app.input().lines().get(app.input().cursor_row())?;
            let current_idx = input::parse_paste_placeholder_before_cursor(
                current_line,
                app.input().cursor_col(),
            )?;
            (current_idx == session_idx).then_some(())
        })
        .is_some()
        && app.input_mut().append_to_active_paste_block(&pasted);
    if appended {
        *app.active_paste_session_mut() = Some(session);
        app.needs_redraw = true;
        tracing::debug!(
            target: crate::logging::targets::APP_PASTE,
            event_name = "paste_placeholder_appended",
            message = "paste content appended to an active placeholder",
            outcome = "success",
            session_id,
            pasted_chars,
        );
        return;
    }

    let char_count = input::count_text_chars(&pasted);
    if char_count > input::PASTE_PLACEHOLDER_CHAR_THRESHOLD {
        app.input_mut().insert_paste_block(&pasted);
        let idx = app.input().lines().get(app.input().cursor_row()).and_then(|line| {
            input::parse_paste_placeholder_before_cursor(line, app.input().cursor_col())
        });
        *app.active_paste_session_mut() =
            Some(state::PasteSessionState { placeholder_index: idx, ..session });
        tracing::debug!(
            target: crate::logging::targets::APP_PASTE,
            event_name = "paste_placeholder_inserted",
            message = "paste content inserted as a placeholder block",
            outcome = "success",
            session_id,
            pasted_chars,
            char_count,
            placeholder_index = ?idx,
        );
    } else {
        app.input_mut().insert_str(&pasted);
        *app.active_paste_session_mut() = None;
        tracing::debug!(
            target: crate::logging::targets::APP_PASTE,
            event_name = "paste_inline_inserted",
            message = "paste content inserted inline",
            outcome = "success",
            session_id,
            pasted_chars,
            char_count,
            lines = app.input().lines().len(),
        );
    }
    app.needs_redraw = true;
}

fn cursor_gt(a: SelectionPoint, b: SelectionPoint) -> bool {
    a.row > b.row || (a.row == b.row && a.col > b.col)
}

fn cursor_to_byte_offset(lines: &[String], cursor: SelectionPoint) -> Option<usize> {
    let line = lines.get(cursor.row)?;
    let mut offset = 0usize;
    for prior in &lines[..cursor.row] {
        offset = offset.saturating_add(prior.len().saturating_add(1));
    }
    Some(offset.saturating_add(char_to_byte_index(line, cursor.col)))
}

fn char_to_byte_index(text: &str, char_idx: usize) -> usize {
    text.char_indices().nth(char_idx).map_or(text.len(), |(i, _)| i)
}

fn byte_offset_to_cursor(text: &str, byte_offset: usize) -> SelectionPoint {
    let mut row = 0usize;
    let mut col = 0usize;
    let mut seen = 0usize;
    for ch in text.chars() {
        let ch_len = ch.len_utf8();
        if seen + ch_len > byte_offset {
            break;
        }
        seen += ch_len;
        if ch == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    SelectionPoint { row, col }
}

fn apply_merged_input_snapshot(app: &mut App, merged: &str, cursor_offset: usize) {
    let mut lines: Vec<String> = merged.split('\n').map(ToOwned::to_owned).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    let mut cursor = byte_offset_to_cursor(merged, cursor_offset.min(merged.len()));
    if cursor.row >= lines.len() {
        cursor.row = lines.len().saturating_sub(1);
        cursor.col = lines[cursor.row].chars().count();
    } else {
        cursor.col = cursor.col.min(lines[cursor.row].chars().count());
    }

    app.input_mut().replace_lines_and_cursor(lines, cursor.row, cursor.col);
}

fn strip_input_range(app: &mut App, start: SelectionPoint, end: SelectionPoint) {
    if cursor_gt(start, end) || start == end {
        return;
    }
    let Some(start_offset) = cursor_to_byte_offset(app.input().lines(), start) else {
        return;
    };
    let Some(end_offset) = cursor_to_byte_offset(app.input().lines(), end) else {
        return;
    };
    if start_offset >= end_offset {
        return;
    }
    let raw = app.input().lines().join("\n");
    if end_offset > raw.len() {
        return;
    }
    let mut merged = String::with_capacity(raw.len().saturating_sub(end_offset - start_offset));
    merged.push_str(&raw[..start_offset]);
    merged.push_str(&raw[end_offset..]);
    apply_merged_input_snapshot(app, &merged, start_offset);
}

/// Finalize a deferred Enter by restoring the exact pre-submit input snapshot
/// and submitting that original draft text.
fn finalize_deferred_submit(app: &mut App) {
    let Some(snapshot) = app.pending_submit_mut().take() else {
        return;
    };
    app.input_mut().restore_snapshot(snapshot);
    input_submit::submit_input(app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::model;

    use crate::app::{MessageBlock, MessageRole};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn app_with_connection()
    -> (App, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>) {
        let mut app = App::test_default();
        let rx = app.install_testing_stub();
        app.set_session_id(Some(model::SessionId::new("session-1")));
        (app, rx)
    }

    #[test]
    fn pending_paste_chunks_are_merged_before_threshold_check() {
        let mut app = App::test_default();
        let first = "a".repeat(700);
        let second = "b".repeat(401);
        events::handle_terminal_event(&mut app, Event::Paste(first.clone()));
        events::handle_terminal_event(&mut app, Event::Paste(second.clone()));

        // Not applied until post-drain finalization.
        assert!(app.input().is_empty());
        assert!(!app.pending_paste_text().is_empty());

        finalize_pending_paste_event(&mut app);

        assert_eq!(app.input().lines(), vec!["[Pasted Text 1 - 1101 chars]"]);
        assert_eq!(app.input().text(), format!("{first}{second}"));
    }

    #[test]
    fn pending_paste_chunk_appends_to_same_session_placeholder() {
        let mut app = App::test_default();
        app.input_mut().insert_paste_block("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk");
        *app.active_paste_session_mut() = Some(state::PasteSessionState {
            id: 7,
            start: SelectionPoint { row: 0, col: 0 },
            placeholder_index: Some(0),
        });
        *app.pending_paste_session_mut() = app.active_paste_session().copied();
        *app.pending_paste_text_mut() = "\nl\nm".to_owned();

        finalize_pending_paste_event(&mut app);

        assert_eq!(app.input().lines(), vec!["[Pasted Text 1 - 25 chars]"]);
        assert_eq!(app.input().text(), "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm");
    }

    #[test]
    fn pending_paste_exact_1000_chars_stays_inline() {
        let mut app = App::test_default();
        *app.pending_paste_text_mut() = "x".repeat(1000);

        finalize_pending_paste_event(&mut app);

        assert_eq!(app.input().lines(), vec!["x".repeat(1000)]);
    }

    #[test]
    fn pending_paste_finalization_marks_redraw() {
        let mut app = App::test_default();
        app.needs_redraw = false;
        *app.pending_paste_text_mut() = "hello\nworld".to_owned();

        finalize_pending_paste_event(&mut app);

        assert!(app.needs_redraw);
        assert_eq!(app.input().lines(), vec!["hello", "world"]);
    }

    #[test]
    fn suppressed_enter_preserves_multiline_inline_paste() {
        let mut app = App::test_default();
        let t0 = Instant::now();

        assert_eq!(app.paste_burst.on_char('a', t0), paste_burst::CharAction::Passthrough('a'));
        let _ = app.input_mut().textarea_insert_char('a');
        assert_eq!(
            app.paste_burst.on_char('b', t0 + Duration::from_millis(2)),
            paste_burst::CharAction::Consumed
        );
        assert_eq!(
            app.paste_burst.on_char('c', t0 + Duration::from_millis(4)),
            paste_burst::CharAction::RetroCapture(1)
        );
        let _ = app.input_mut().textarea_delete_char_before();

        let t_flush = t0 + Duration::from_millis(200);
        assert_eq!(
            app.paste_burst.tick(t_flush),
            Some(paste_burst::FlushAction::EmitPaste("abc".to_owned()))
        );
        app.queue_paste_text("abc");
        finalize_pending_paste_event(&mut app);
        assert_eq!(app.input().text(), "abc");

        let t_enter = t_flush + Duration::from_millis(10);
        assert!(app.paste_burst.on_enter(t_enter));
        assert_eq!(
            app.paste_burst.on_char('d', t_enter + Duration::from_millis(1)),
            paste_burst::CharAction::Consumed
        );
        assert_eq!(
            app.paste_burst.on_char('e', t_enter + Duration::from_millis(2)),
            paste_burst::CharAction::Consumed
        );
        assert_eq!(
            app.paste_burst.on_char('f', t_enter + Duration::from_millis(3)),
            paste_burst::CharAction::Consumed
        );

        let t_second_flush = t_enter + Duration::from_millis(200);
        assert_eq!(
            app.paste_burst.tick(t_second_flush),
            Some(paste_burst::FlushAction::EmitPaste("\ndef".to_owned()))
        );
        app.queue_paste_text("\ndef");
        finalize_pending_paste_event(&mut app);

        assert_eq!(app.input().lines(), vec!["abc", "def"]);
        assert_eq!(app.input().text(), "abc\ndef");
    }

    #[test]
    fn pending_paste_1001_chars_becomes_placeholder() {
        let mut app = App::test_default();
        *app.pending_paste_text_mut() = "x".repeat(1001);

        finalize_pending_paste_event(&mut app);

        assert_eq!(app.input().lines(), vec!["[Pasted Text 1 - 1001 chars]"]);
        assert_eq!(app.input().text(), "x".repeat(1001));
    }

    #[test]
    fn pending_paste_session_isolation_prevents_unintended_append() {
        let mut app = App::test_default();
        *app.pending_paste_text_mut() = "a".repeat(1001);
        finalize_pending_paste_event(&mut app);
        events::handle_terminal_event(
            &mut app,
            Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('v'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
        );

        *app.pending_paste_text_mut() = "b".repeat(1001);
        finalize_pending_paste_event(&mut app);

        assert_eq!(
            app.input().lines(),
            vec!["[Pasted Text 1 - 1001 chars][Pasted Text 2 - 1001 chars]"]
        );
        assert_eq!(app.input().text(), format!("{}{}", "a".repeat(1001), "b".repeat(1001)));
    }

    #[test]
    fn plain_enter_preserves_single_line_draft_before_submit() {
        let (mut app, mut rx) = app_with_connection();
        app.input_mut().set_text("hello world");
        let _ = app.input_mut().set_cursor(0, "hello".chars().count());

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(app.input().text(), "hello world");
        assert_eq!(app.input().cursor(), (0, "hello".chars().count()));
        assert!(app.pending_submit().is_some());

        finalize_deferred_submit(&mut app);

        assert!(app.pending_submit().is_none());
        assert!(app.input().text().is_empty());
        assert_eq!(app.messages().len(), 2);
        assert!(matches!(app.messages()[0].role, MessageRole::User));
        assert!(matches!(
            app.messages()[0].blocks.as_slice(),
            [MessageBlock::Text(block)] if block.text == "hello world"
        ));
        let envelope = rx.try_recv().expect("prompt command should be sent");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::PromptWithImages { session_id, .. } if session_id == "session-1"
        ));
    }

    #[test]
    fn plain_enter_preserves_multiline_draft_with_mid_buffer_cursor() {
        let (mut app, mut rx) = app_with_connection();
        app.input_mut().set_text("alpha beta\ngamma");
        let _ = app.input_mut().set_cursor(0, "alpha".chars().count());

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(app.input().text(), "alpha beta\ngamma");
        assert_eq!(app.input().cursor(), (0, "alpha".chars().count()));
        assert!(app.pending_submit().is_some());

        finalize_deferred_submit(&mut app);

        assert!(app.pending_submit().is_none());
        assert!(matches!(
            app.messages()[0].blocks.as_slice(),
            [MessageBlock::Text(block)] if block.text == "alpha beta\ngamma"
        ));
        let envelope = rx.try_recv().expect("prompt command should be sent");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::PromptWithImages { session_id, .. } if session_id == "session-1"
        ));
    }

    #[test]
    fn sending_lone_question_mark_closes_help_overlay() {
        let (mut app, mut rx) = app_with_connection();

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
        );

        assert_eq!(app.input().text(), "?");
        assert!(app.is_help_active());

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(app.pending_submit().is_some());

        finalize_deferred_submit(&mut app);

        assert!(app.pending_submit().is_none());
        assert!(app.input().text().is_empty());
        assert!(!app.is_help_active());
        assert!(matches!(
            app.messages()[0].blocks.as_slice(),
            [MessageBlock::Text(block)] if block.text == "?"
        ));
        let envelope = rx.try_recv().expect("prompt command should be sent");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::PromptWithImages { session_id, .. } if session_id == "session-1"
        ));
    }

    #[test]
    fn mode_selection_then_second_enter_arms_submit() {
        let mut app = App::test_default();
        app.set_mode(Some(ModeState {
            current_mode_id: "code".to_owned(),
            current_mode_name: "Code".to_owned(),
            available_modes: vec![
                ModeInfo { id: "plan".to_owned(), name: "Plan".to_owned(), description: None },
                ModeInfo { id: "code".to_owned(), name: "Code".to_owned(), description: None },
            ],
        }));
        app.input_mut().set_text("/mode pl");
        let _ = app.input_mut().set_cursor(0, "/mode pl".chars().count());
        crate::app::slash::sync_with_cursor(&mut app);

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(app.input().text(), "/mode plan ");
        assert!(app.slash().is_none());
        assert_eq!(app.focus_owner(), FocusOwner::Input);

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert!(app.pending_submit().is_some());
    }

    #[test]
    fn model_selection_then_second_enter_arms_submit() {
        let mut app = App::test_default();
        app.try_active_bucket_mut().unwrap().available_models = vec![
            model::AvailableModel::new("sonnet", "Claude Sonnet"),
            model::AvailableModel::new("haiku", "Claude Haiku"),
        ];
        app.input_mut().set_text("/model so");
        let _ = app.input_mut().set_cursor(0, "/model so".chars().count());
        crate::app::slash::sync_with_cursor(&mut app);

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(app.input().text(), "/model sonnet ");
        assert!(app.slash().is_none());
        assert_eq!(app.focus_owner(), FocusOwner::Input);

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert!(app.pending_submit().is_some());
    }

    #[test]
    fn resume_selection_then_second_enter_arms_submit() {
        let mut app = App::test_default();
        *app.recent_sessions_mut() = vec![RecentSessionInfo {
            session_id: "session-1".to_owned(),
            summary: "Session one".to_owned(),
            last_modified_ms: 1,
            file_size_bytes: 1,
            cwd: None,
            git_branch: None,
            custom_title: None,
            first_prompt: None,
        }];
        app.input_mut().set_text("/resume se");
        let _ = app.input_mut().set_cursor(0, "/resume se".chars().count());
        crate::app::slash::sync_with_cursor(&mut app);

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert_eq!(app.input().text(), "/resume session-1 ");
        assert!(app.slash().is_none());
        assert_eq!(app.focus_owner(), FocusOwner::Input);

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert!(app.pending_submit().is_some());
    }

    #[test]
    fn paste_event_cancels_deferred_submit_snapshot() {
        let mut app = App::test_default();
        app.input_mut().set_text("draft");

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(app.pending_submit().is_some());

        events::handle_terminal_event(&mut app, Event::Paste("pasted".into()));

        assert!(app.pending_submit().is_none());
        assert_eq!(app.pending_paste_text(), "pasted");
        assert_eq!(app.input().text(), "draft");
    }

    #[test]
    fn esc_cancels_deferred_submit_snapshot_before_finalize() {
        let (mut app, mut rx) = app_with_connection();
        app.input_mut().set_text("draft");

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(app.pending_submit().is_some());

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );

        assert!(app.pending_submit().is_none());
        finalize_deferred_submit(&mut app);
        assert_eq!(app.input().text(), "draft");
        assert!(app.messages().is_empty());
        assert!(rx.try_recv().is_err(), "Esc should prevent deferred submit dispatch");
    }

    #[test]
    fn spinner_advances_less_frequently_when_reduced_motion_enabled() {
        let mut app = App::test_default();
        let base = Instant::now();

        advance_spinner_frame(&mut app, base);
        assert_eq!(app.spinner_frame, 1);
        advance_spinner_frame(&mut app, base + Duration::from_millis(40));
        assert_eq!(app.spinner_frame, 2);

        crate::app::config::store::set_prefers_reduced_motion(
            &mut app.config.committed_local_settings_document,
            true,
        );
        app.spinner_last_advance_at = None;
        app.spinner_frame = 0;

        advance_spinner_frame(&mut app, base);
        assert_eq!(app.spinner_frame, 1);
        advance_spinner_frame(&mut app, base + Duration::from_millis(95));
        assert_eq!(app.spinner_frame, 1);
        advance_spinner_frame(&mut app, base + Duration::from_millis(121));
        assert_eq!(app.spinner_frame, 2);
    }
}
