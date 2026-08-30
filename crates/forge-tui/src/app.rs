pub(crate) mod account_picker;
pub(crate) mod active_bucket_scope;
mod cache_policy;
pub(crate) mod cli_version;
pub(crate) mod clipboard_image;
pub(crate) mod config;
pub(crate) mod connect;
mod dialog;
pub(crate) mod diff_overlay;
pub(crate) mod emoji;
pub(crate) mod events;
pub(crate) mod file_index;
mod focus;
pub(crate) mod git_diff;
pub(crate) mod input;
mod input_submit;
mod keys;
pub(crate) mod launchpad;
pub(crate) mod mention;
pub(crate) mod monitor_output;
mod notify;
pub(crate) mod paste_burst;
pub(crate) mod plugins;
pub mod preflight;
pub(crate) mod process_scanner;
pub(crate) mod processes;
pub(crate) mod prompt;
#[cfg(test)]
pub(crate) mod replay;
pub(crate) mod review_waiting;
mod selection;
mod service_status_check;
pub mod session;
mod session_runtime;
pub(crate) mod slash;
pub(crate) mod spinner_picker;
mod state;
pub(crate) mod subagent;
mod tab_title;
mod terminal;
mod todos;
pub(crate) mod usage;
pub(crate) mod usage_overlay;
pub(crate) mod view;

// Re-export all public types so `crate::app::App`, `crate::app::BlockCache`, etc. still work.
pub use cache_policy::{
    CacheSplitPolicy, DEFAULT_CACHE_SPLIT_HARD_LIMIT_BYTES, DEFAULT_CACHE_SPLIT_SOFT_LIMIT_BYTES,
    TextSplitDecision, TextSplitKind, default_cache_split_policy, find_text_split,
    find_text_split_index,
};
pub use config::ConfigState;
pub use connect::{create_app, start_connection};
pub use diff_overlay::DiffOverlayState;
pub use emoji::{Emoji, EmojiState};
pub use events::{apply_session_update, handle_terminal_event};
pub use focus::{FocusManager, FocusOwner, FocusTarget};
pub use input::{InputState, TypedChar};
pub use launchpad::LaunchpadState;
pub use prompt::{PromptMode, PromptSource, PromptState};
pub(crate) use selection::normalize_selection;
pub use service_status_check::start_service_status_check;
pub use spinner_picker::SpinnerPickerState;
pub(crate) use state::MarkdownRenderKey;
pub(crate) use state::cache_metrics;
pub use state::{
    AnsweredQuestion, App, AppStatus, AttentionEntry, AttentionKind, BackgroundTask, BlockCache,
    CacheMetrics, CachedMessageSegment, ChatMessage, ChatRenderTraceState, ChatViewport,
    ExtraUsage, FailedTurn, HelpView, IncrementalMarkdown, InputFocus, InvalidationLevel,
    LayoutInvalidation, LoginHint, McpState, MessageBlock, MessageRenderCache,
    MessageRenderCacheKey, MessageRenderSignature, MessageRole, ModeInfo, ModeState, MonitorEntry,
    MonitorStatus, NoticeBlock, NoticeDedupKey, NoticeStage, PaneHitTarget, PasteSessionState,
    PendingCommandAck, PhaseEntry, PhaseStatus, RateLimitIncidentKey, RecentSessionInfo,
    ReviewRepliesWaiting, SUBAGENT_TAIL_CAP, ScheduleEntry, ScheduleKind, ScrollbarGeometry,
    SelectionKind, SelectionPoint, SelectionState, SessionTurnState, SessionUsageState,
    StopHookEntry, StopHookSummaryState, SubagentChildEntry, SubagentEntry, SystemSeverity,
    TerminalSnapshotMode, TextBlock, TextBlockSpacing, TodoItem, TodoStatus, ToolCallInfo,
    ToolCallScope, TurnNoticeLocation, TurnNoticeRef, UsageSnapshot, UsageSourceKind, UsageState,
    UsageWindow, WelcomeBlock, WorkflowEntry, WorkflowStatus, compute_scrollbar_geometry,
    hash_text_block_content, hash_welcome_block_content, is_execute_tool_name,
    is_monitor_tool_name,
};
pub use usage_overlay::UsageOverlayState;
pub use view::ActiveView;

use crossterm::event::{
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use futures::StreamExt;
use std::time::{Duration, Instant};

/// Repaint and pulse cadence under a reduced-motion preference. Fixed
/// rather than derived from `[ui] fps`: the point of reduced motion is
/// fewer frames, so a high frame rate must not pull it up.
const SPINNER_FRAME_INTERVAL_REDUCED: Duration = Duration::from_millis(120);

/// Step interval for [`App::spinner_frame`], pinned rather than
/// following `[ui] fps`. Its one consumer is not a spinner and does not
/// scale: the tab-title pulse alternates two glyphs every ten steps, so
/// driven off a fast repaint rate it reads as flicker rather than
/// motion. Also the coarsest interval the repaint gate can use, see
/// `forge_workspace::ui::COARSEST_REPAINT_INTERVAL`.
const PULSE_INTERVAL: Duration = Duration::from_millis(30);

/// Loop wake interval, tightened by [`loop_tick`] above 120fps.
const LOOP_TICK: Duration = Duration::from_millis(4);

/// Hard cap on candidates shown in autocomplete dropdowns
/// (file_index, slash, subagent). The slash dropdown sees the
/// largest counts (forge group + every claude slash command + every
/// installed skill / plugin command); 50 was hitting that ceiling
/// in real use. 200 covers anything realistic with headroom; the
/// dropdown's own visible-row cap (slash::MAX_VISIBLE = 20) keeps
/// the on-screen size bounded regardless.
pub(crate) const MAX_CANDIDATES: usize = 200;

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
    // Turn off any-motion tracking (1003) and reset the OS pointer to
    // the arrow so an exited forge / child process doesn't inherit a
    // stale shape.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::style::Print("\x1b[?1003l\x1b]22;default\x07")
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Wrap ratatui's restore-on-panic hook so a *caught* render panic
/// can't corrupt the live TUI.
///
/// ratatui's hook runs at the panic site, before the `catch_unwind` in
/// `ui::markdown::render_markdown_safe` swallows a markdown panic, and
/// restores the terminal (raw mode off, leave alt-screen). That alone
/// flips the terminal to cooked mode (which echoes input, rendering ESC
/// as `^[`) while forge's mouse capture keeps streaming, leaking SGR
/// motion sequences onto the screen. So: a panic inside the markdown
/// guard is left untouched for `catch_unwind`; any other panic is a real
/// crash and gets forge's full teardown (which also disables mouse
/// capture, unlike ratatui's restore) before the captured hook prints
/// the backtrace.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if crate::ui::markdown::in_guarded_render() {
            return;
        }
        suspend_terminal();
        previous(info);
    }));
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
    // Any-motion mouse tracking (1003) so forge receives hover-move
    // events, not just drags - needed for the pointer-shape affordance.
    // crossterm's EnableMouseCapture only sets 1000/1002/1006.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::style::Print("\x1b[?1003h"));
}

/// Emit the OSC 22 pointer-shape sequence iff the desired shape changed
/// since the last write. Called once per loop pass, de-duped so a still
/// pointer costs nothing - and never touches the ratatui frame (hover
/// is a terminal side-channel, off the render path).
fn flush_pointer_shape(app: &mut App) {
    if let Some(osc) = crate::app::events::mouse::take_pointer_shape_emit(app) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::style::Print(osc));
    }
}

// ---------------------------------------------------------------------------
// TUI event loop
// ---------------------------------------------------------------------------

pub async fn run_tui(app: &mut App) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let mut os_shutdown = Box::pin(wait_for_shutdown_signal());

    // Enable bracketed paste, mouse capture, and enhanced keyboard protocol
    resume_terminal();

    // Wrap ratatui's restore-on-panic hook so a caught markdown render
    // panic can't flip the terminal out of raw mode mid-session.
    install_panic_hook();

    let mut events = EventStream::new();
    // Measured from the last render, so it only bounds the first wake
    // after one: past that the sleep collapses to zero and the loop
    // re-enters `select!` at the timer's own granularity. Idle costs
    // nothing regardless because the render is skipped when
    // `needs_redraw == false`.
    let tick_duration = loop_tick(app.repaint_cadence.frame_interval());
    let mut last_render = Instant::now();
    let mut last_spinner_repaint_tick = spinner_repaint_tick(app);

    loop {
        start_connection(app);

        // Wait for an event or the next frame tick.
        let time_to_next = tick_duration.saturating_sub(last_render.elapsed());
        // Measured on the select arm because that is now the only path
        // terminal events take.
        let mut input_ms = 0.0;
        // Accumulates across both apply sites - the select arm below
        // and the drain loop - because the first update of a burst
        // lands on the arm while the loop is parked on `recv`, and a
        // slow one there blocks the loop exactly as it would in the
        // drain.
        let mut updates_ms = 0.0;
        tokio::select! {
            Some(Ok(event)) = events.next() => {
                let input_start = crate::perf::phase_start();
                events::handle_terminal_event(app, event);
                input_ms = crate::perf::phase_ms(input_start);
            }
            Some(update) = app.update_rx.recv() => {
                let update_start = crate::perf::phase_start();
                events::apply_session_update(app, update);
                updates_ms = crate::perf::phase_ms(update_start);
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

        // Drain queued session updates without blocking. Terminal
        // events stay on the select arm: polling the crossterm stream
        // here supplies a noop waker, which strands its wake thread on
        // the internal reader lock and stalls this loop for tens of ms.
        let drain_start = crate::perf::phase_start();
        while let Ok(update) = app.update_rx.try_recv() {
            let update_start = crate::perf::phase_start();
            events::apply_session_update(app, update);
            updates_ms += crate::perf::phase_ms(update_start);
        }

        file_index::drain_events(app);
        git_diff::drain_events(app);
        cli_version::drain_events(app);
        process_scanner::drain_events(app);
        diff_overlay::drain_events(app);
        review_waiting::drain_events(app);
        usage_overlay::drain_events(app);

        // If a prior turn ended in Error state because of a rate-limit
        // rejection, drop the input lock once the rate-limit window
        // has passed. The CLI doesn't proactively emit RateLimitEvent
        // updates without a fresh request, so we poll the wall clock
        // each tick instead of relying on the wire.
        events::rate_limit::maybe_recover_from_rate_limit_lock(app);

        // Same shape one tier down: a turn killed by a transient server
        // error gets a forge-sent continuation once its backoff elapses.
        // Wall-clock polled per tick for the same reason - nothing
        // arrives on the wire to tell us the delay is up.
        events::auto_continue::maybe_fire(app);

        // Preflight hands over to wherever the user was headed, and a
        // cancelled model fetch quits once its screen has been painted.
        // Driven from here rather than from the renderer, since handing
        // over is a view transition.
        crate::app::preflight::tick(app);

        // The Projects pane's account/status panel renders 5h + 7d
        // usage bars on every frame. Keep the snapshot live by
        // calling `request_refresh_if_needed` each tick - it's
        // idempotent (no-ops while the existing snapshot is younger
        // than `USAGE_REFRESH_TTL` and while a request is in flight),
        // so the actual fetch only fires once per TTL window.
        crate::app::usage::request_refresh_if_needed(app);

        // The CLI reports every server as `pending` with no handshake for
        // the first moments after connect, so the connect-time fetch alone
        // would freeze that pre-handshake state for the session's life.
        crate::app::config::request_mcp_snapshot_if_needed(app, Instant::now());

        // Tick the burst detector: flush any held/buffered content
        // that has timed out. Routed by which editor has focus, not by
        // view - the /diff review editors take dictation too.
        flush_paste_burst(app, Instant::now());

        // Merge and process `Event::Paste` chunks as one paste action.
        if app.has_focused_text_input() && !app.pending_paste_text().is_empty() {
            finalize_pending_paste_event(app);
        }

        // Deferred submit: if Enter was pressed and no paste payload arrived
        // in this drain cycle, restore the exact pre-submit snapshot and
        // submit that unchanged draft.
        if app.active_view == ActiveView::Chat && app.pending_submit().is_some() {
            finalize_deferred_submit(app);
        }

        // Flush the desired OS pointer shape (off the render path, every
        // pass, de-duped) so hover updates the cursor without a redraw.
        flush_pointer_shape(app);

        if app.should_quit {
            break;
        }

        // Render once, only when something changed - and keep
        // rendering while anything is happening anywhere, so a
        // background session's row spinner advances even when the
        // active session is idle.
        let is_animating = is_animating(app);
        if is_animating {
            advance_spinner_frame(app, Instant::now());
            tab_title::update_tab_title(is_animating, app.spinner_frame, app.cwd());
            // The loop wakes more often than the frame interval, so
            // repainting per wake would redraw an unchanged frame
            // several times over.
            let tick = spinner_repaint_tick(app);
            if tick != last_spinner_repaint_tick {
                last_spinner_repaint_tick = tick;
                app.needs_redraw = true;
            }
        } else {
            app.spinner_last_advance_at = None;
        }
        // Catch the transition into stillness, which the branch above cannot.
        if !is_animating && app.needs_redraw {
            tab_title::update_tab_title(is_animating, app.spinner_frame, app.cwd());
        }
        // Smooth scroll still settling - viewport row index (usize)
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
        let drain_ms = crate::perf::phase_ms(drain_start);
        let mut render_ms = None;
        if app.needs_redraw {
            let render_start = crate::perf::phase_start();
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
            render_ms = Some(crate::perf::phase_ms(render_start));
            app.needs_redraw = false;
            last_render = Instant::now();
        }
        crate::perf::record_iteration(&crate::perf::IterationCost {
            drain_ms,
            input_ms,
            updates_ms,
            render_ms,
            animating: is_animating,
        });
    }

    // --- Graceful shutdown ---

    // Cancel all queued prompts via the workspace dispatch path. The
    // workspace-side `SessionTask` pops the matching slot and the
    // bridge forwards `Cancelled` to the agent.
    let active_session_key = app.active_session_key.clone();
    if let Some(session_key) = active_session_key.as_ref() {
        let queued: Vec<crate::app::prompt::PromptState> =
            if let Some(session) = app.session_mut(session_key) {
                session.prompt_queue.drain(..).collect()
            } else {
                Vec::new()
            };
        for prompt in queued {
            match prompt.source {
                crate::app::prompt::PromptSource::Permission { .. } => {
                    crate::app::events::turn::dispatch_permission_outcome(
                        app,
                        session_key,
                        &prompt.tool_id,
                        forge_primitives::PermissionOutcome::Cancelled,
                    );
                }
                crate::app::prompt::PromptSource::Question { .. } => {
                    crate::app::events::turn::dispatch_question_outcome(
                        app,
                        session_key,
                        &prompt.tool_id,
                        forge_primitives::QuestionOutcome::Cancelled,
                    );
                }
            }
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

/// Whether anything on screen is mid-animation, which is what earns a
/// repaint on a tick nothing else changed.
///
/// The preflight clause is not decoration. Account state and dictation
/// progress are both POLLED by the renderer rather than pushed, so
/// neither emits a `SessionUpdate` and neither marks `needs_redraw`; and
/// [`App::shows_activity`] reads `app.sessions`, which is empty at boot.
/// Without this, preflight paints once with a spinner that never moves
/// and a screen that never updates while 3.07 GB downloads.
pub(crate) fn is_animating(app: &App) -> bool {
    if app.active_view == crate::app::ActiveView::Launchpad && !app.preflight_done {
        return true;
    }
    app.shows_activity()
}

/// Which animation step the spinner epoch is on. Rendered glyphs divide
/// that same epoch by their style cadence, so gating repaints on this
/// cannot drift against them the way a separately-accumulated counter
/// would; `repaint_interval` is no coarser than the quickest cadence, so
/// no animated surface can change between two consecutive ticks.
fn spinner_repaint_tick(app: &App) -> u128 {
    spinner_animation_step(app.spinner_epoch.elapsed(), repaint_interval(app))
}

fn spinner_animation_step(elapsed: Duration, interval: Duration) -> u128 {
    elapsed.as_micros() / interval.as_micros().max(1)
}

/// Interval between repaints while animating - the `[ui] fps` setting.
fn repaint_interval(app: &App) -> Duration {
    if app.config.prefers_reduced_motion_effective() {
        SPINNER_FRAME_INTERVAL_REDUCED
    } else {
        app.repaint_cadence.frame_interval()
    }
}

/// Interval between [`App::spinner_frame`] steps. Deliberately not the
/// repaint interval - see [`PULSE_INTERVAL`].
fn pulse_interval(app: &App) -> Duration {
    if app.config.prefers_reduced_motion_effective() {
        SPINNER_FRAME_INTERVAL_REDUCED
    } else {
        PULSE_INTERVAL
    }
}

/// Loop wake interval for a given frame interval. A wake cadence coarser
/// than half the frame interval cannot land near a frame boundary, so a
/// high `[ui] fps` tightens it; at the default it stays [`LOOP_TICK`].
fn loop_tick(frame_interval: Duration) -> Duration {
    LOOP_TICK.min(frame_interval / 2)
}

fn advance_spinner_frame(app: &mut App, now: Instant) {
    let interval = pulse_interval(app);

    match app.spinner_last_advance_at {
        Some(last_advance) if now.duration_since(last_advance) < interval => {}
        Some(_) | None => {
            app.spinner_frame = app.spinner_frame.wrapping_add(1);
            app.spinner_last_advance_at = Some(now);
        }
    }
}

async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        sigint = tokio::signal::ctrl_c() => {
            sigint?;
        }
        _ = sigterm.recv() => {}
    }
    Ok(())
}

/// Flush a timed-out paste burst into the focused editor. `EmitChar`
/// re-inserts a single held character; `EmitPaste` feeds the accumulated
/// burst into the paste queue. A burst with no focused editor is dropped
/// on the floor - there is nowhere for it to land.
fn flush_paste_burst(app: &mut App, now: Instant) {
    if !app.has_focused_text_input() {
        return;
    }
    match app.paste_burst.tick(now) {
        Some(paste_burst::FlushAction::EmitChar(ch)) => {
            if let Some(input) = app.focused_input_mut() {
                let _ = input.textarea_insert_char(ch);
            }
        }
        Some(paste_burst::FlushAction::EmitPaste(text)) => app.queue_paste_text(&text),
        None => {}
    }
}

/// Finalize queued `Event::Paste` chunks for this drain cycle.
fn finalize_pending_paste_event(app: &mut App) {
    let pasted = std::mem::take(app.pending_paste_text_mut());
    if pasted.is_empty() {
        return;
    }
    let Some(cursor) = focused_cursor(app) else {
        return;
    };
    let pasted_chars = pasted.chars().count();

    let session = app.pending_paste_session_mut().take().unwrap_or_else(|| {
        let id = app.allocate_paste_session_id();
        state::PasteSessionState { id, start: cursor, placeholder_index: None }
    });
    let session_id = session.id;

    if session.placeholder_index.is_none() {
        strip_input_range(app, session.start, cursor);
    }

    let appended = session
        .placeholder_index
        .and_then(|session_idx| {
            let input = app.focused_input()?;
            let current_line = input.lines().get(input.cursor_row())?;
            let current_idx =
                input::parse_paste_placeholder_before_cursor(current_line, input.cursor_col())?;
            (current_idx == session_idx).then_some(())
        })
        .is_some()
        && app.focused_input_mut().is_some_and(|input| input.append_to_active_paste_block(&pasted));
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
    let line_count = pasted.split(['\n', '\r']).count();
    if char_count > input::PASTE_PLACEHOLDER_CHAR_THRESHOLD
        || line_count > input::PASTE_PLACEHOLDER_LINE_THRESHOLD
    {
        if let Some(input) = app.focused_input_mut() {
            input.insert_paste_block(&pasted);
        }
        let idx = app.focused_input().and_then(|input| {
            let line = input.lines().get(input.cursor_row())?;
            input::parse_paste_placeholder_before_cursor(line, input.cursor_col())
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
        if let Some(input) = app.focused_input_mut() {
            input.insert_str(&pasted);
        }
        *app.active_paste_session_mut() = None;
        tracing::debug!(
            target: crate::logging::targets::APP_PASTE,
            event_name = "paste_inline_inserted",
            message = "paste content inserted inline",
            outcome = "success",
            session_id,
            pasted_chars,
            char_count,
            lines = app.focused_input().map_or(0, |input| input.lines().len()),
        );
    }
    app.needs_redraw = true;
}

/// Cursor position in the focused editor, or `None` when no editor has
/// focus.
fn focused_cursor(app: &App) -> Option<SelectionPoint> {
    app.focused_input()
        .map(|input| SelectionPoint { row: input.cursor_row(), col: input.cursor_col() })
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

    if let Some(input) = app.focused_input_mut() {
        input.replace_lines_and_cursor(lines, cursor.row, cursor.col);
    }
}

fn strip_input_range(app: &mut App, start: SelectionPoint, end: SelectionPoint) {
    if cursor_gt(start, end) || start == end {
        return;
    }
    let Some(input) = app.focused_input() else {
        return;
    };
    let Some(start_offset) = cursor_to_byte_offset(input.lines(), start) else {
        return;
    };
    let Some(end_offset) = cursor_to_byte_offset(input.lines(), end) else {
        return;
    };
    if start_offset >= end_offset {
        return;
    }
    let raw = input.lines().join("\n");
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
    use forge_workspace::RepaintCadence;

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
    fn pending_paste_six_lines_under_char_threshold_collapses_to_placeholder() {
        // Six short lines: ~12 chars total, well under the 1000-char
        // threshold, but still over the 5-line threshold. Should
        // collapse to a placeholder so the input box doesn't grow tall.
        let mut app = App::test_default();
        *app.pending_paste_text_mut() = "a\nb\nc\nd\ne\nf".to_owned();

        finalize_pending_paste_event(&mut app);

        assert_eq!(app.input().lines(), vec!["[Pasted Text 1 - 11 chars]"]);
    }

    #[test]
    fn pending_paste_five_lines_stays_inline() {
        // Five lines fits inline (right at the threshold = 5).
        let mut app = App::test_default();
        *app.pending_paste_text_mut() = "a\nb\nc\nd\ne".to_owned();

        finalize_pending_paste_event(&mut app);

        assert_eq!(app.input().lines(), vec!["a", "b", "c", "d", "e"]);
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

    /// Diff view with a comment editor open at line 0 of the first
    /// file - the shape `open_input_for_key` builds on a line click.
    fn app_with_open_comment_editor() -> App {
        use crate::app::diff_overlay::{ActiveCommentInput, LineKey};
        let mut app = App::test_default();
        let mut overlay = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![forge_workspace::env::git_diff::hunks::FileHunks {
                path: "a.rs".into(),
                status: forge_workspace::env::git_diff::hunks::FileStatus::Modified,
                hunks: vec![],
                oversize: false,
            }],
        );
        overlay.active_input = Some(ActiveCommentInput {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            editor: InputState::new(),
            prior_comment: None,
            edit_turn: None,
        });
        app.diff_overlay = Some(overlay);
        view::set_active_view(&mut app, ActiveView::Diff);
        app
    }

    fn comment_editor_text(app: &App) -> String {
        app.diff_overlay
            .as_ref()
            .and_then(|o| o.active_input.as_ref())
            .expect("comment editor open")
            .editor
            .lines()
            .join("\n")
    }

    #[test]
    fn bracketed_paste_reaches_the_open_comment_editor() {
        let mut app = app_with_open_comment_editor();

        events::handle_terminal_event(&mut app, Event::Paste("pasted note".to_owned()));
        finalize_pending_paste_event(&mut app);

        assert_eq!(comment_editor_text(&app), "pasted note");
        assert!(app.input().is_empty(), "the chat draft must not absorb a review paste");
    }

    /// The review editors get the chat draft's paste-block treatment: a
    /// big paste collapses to one placeholder row rather than unrolling
    /// hundreds of lines into the comment box, and expands again on save.
    #[test]
    fn large_paste_collapses_to_a_block_in_the_comment_editor() {
        let mut app = app_with_open_comment_editor();
        let big = "x".repeat(1001);

        events::handle_terminal_event(&mut app, Event::Paste(big.clone()));
        finalize_pending_paste_event(&mut app);

        let editor = app
            .diff_overlay
            .as_ref()
            .and_then(|o| o.active_input.as_ref())
            .expect("comment editor open");
        assert_eq!(editor.editor.lines(), vec!["[Pasted Text 1 - 1001 chars]"]);
        assert_eq!(editor.editor.text(), big, "the block expands back on read");
    }

    #[test]
    fn bracketed_paste_reaches_the_finish_review_editor() {
        use crate::app::diff_overlay::FinishReviewState;
        let mut app = app_with_open_comment_editor();
        if let Some(o) = app.diff_overlay.as_mut() {
            o.active_input = None;
            o.finish_review = Some(FinishReviewState { editor: InputState::new() });
        }

        events::handle_terminal_event(&mut app, Event::Paste("overview text".to_owned()));
        finalize_pending_paste_event(&mut app);

        let overview = app
            .diff_overlay
            .as_ref()
            .and_then(|o| o.finish_review.as_ref())
            .expect("finish-review modal open")
            .editor
            .lines()
            .join("\n");
        assert_eq!(overview, "overview text");
        assert!(app.input().is_empty(), "the chat draft must not absorb a review paste");
    }

    /// A dictation burst (speech-to-text delivers one keystroke per
    /// character) has to coalesce into the review editor, not the chat
    /// draft, and not get dropped.
    #[test]
    fn paste_burst_flush_reaches_the_open_comment_editor() {
        let mut app = app_with_open_comment_editor();

        // Machine-speed keystrokes, exactly how dictation arrives.
        for ch in ['h', 'i', '!'] {
            events::handle_terminal_event(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
            );
        }
        assert!(app.paste_burst.is_buffering(), "the diff view must feed the burst detector");

        flush_paste_burst(&mut app, Instant::now() + Duration::from_millis(200));
        finalize_pending_paste_event(&mut app);

        assert_eq!(comment_editor_text(&app), "hi!");
        assert!(app.input().is_empty(), "the chat draft must not absorb dictated review text");
    }

    /// The prompt's notes field writes into the same `App.input` a
    /// bracketed paste already reaches, so a keystroke-delivered paste
    /// has to coalesce there too rather than arriving character by
    /// character.
    #[test]
    fn paste_burst_flush_reaches_the_prompt_notes_field() {
        let mut app = crate::app::prompt::tests::app_with_focused_notes();

        for ch in ['h', 'i', '!'] {
            events::handle_terminal_event(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
            );
        }
        assert!(app.paste_burst.is_buffering(), "the notes field must feed the burst detector");

        flush_paste_burst(&mut app, Instant::now() + Duration::from_millis(200));
        finalize_pending_paste_event(&mut app);

        assert_eq!(app.input().text(), "hi!", "the coalesced burst lands in the notes editor");
    }

    #[test]
    fn deferred_submit_stays_chat_only() {
        let mut app = app_with_open_comment_editor();

        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        );
        events::handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        assert!(
            app.pending_submit().is_none(),
            "Enter in a review editor must never arm a chat prompt submit"
        );
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
            cwd: None,
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
    fn animation_gate_reflects_live_background_work() {
        use crate::app::session::{SessionLifecycleState, UiSession};

        let mut app = App::test_default();
        app.sessions.clear();

        let key = forge_workspace::SessionKey::from_session_id("bg-gate");
        let mut session = UiSession::new(key.clone());
        session.lifecycle_state = SessionLifecycleState::Idle;
        app.sessions.insert(key.clone(), session);

        // Idle with no background work: nothing to animate.
        assert!(!app.shows_activity(), "idle with no background work must not animate");

        // A live backgrounded task promotes the gate so the frame ticker
        // keeps advancing for a row that's active only because of it.
        app.sessions.get_mut(&key).expect("bucket").background_tasks.push(
            crate::app::BackgroundTask {
                task_id: "t1".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "cargo build".to_owned(),
            },
        );
        assert!(app.shows_activity(), "idle with live background work must animate");
    }

    /// The gate must count exactly the rows that spin: an Attention session
    /// shows a triangle (not a spinner) even with a live backgrounded task,
    /// so it must NOT tick the frame - otherwise we burn redraws animating a
    /// static glyph. A Running session always spins and ticks.
    #[test]
    fn animation_gate_counts_only_spinning_rows() {
        use crate::app::session::{SessionLifecycleState, UiSession};

        let mut app = App::test_default();
        app.sessions.clear();

        let key = forge_workspace::SessionKey::from_session_id("gate-match");
        let mut session = UiSession::new(key.clone());
        session.lifecycle_state = SessionLifecycleState::Attention;
        session.background_tasks.push(crate::app::BackgroundTask {
            task_id: "t1".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "gh run watch".to_owned(),
        });
        app.sessions.insert(key.clone(), session);
        assert!(
            !app.shows_activity(),
            "Attention + background work shows a triangle, not a spinner - gate must not tick",
        );

        app.sessions.get_mut(&key).expect("bucket").lifecycle_state =
            SessionLifecycleState::Running;
        assert!(app.shows_activity(), "a Running session spins, so the gate ticks");
    }

    /// Compaction is activity even though it is not a turn: the chat shows
    /// a compacting line, so the title and the ticker must agree with it.
    #[test]
    fn animation_gate_counts_compaction() {
        let mut app = App::test_default();
        assert!(!app.shows_activity(), "idle to begin with");

        app.set_is_compacting(true);
        assert!(app.shows_activity(), "a compacting session is busy");

        app.set_is_compacting(false);
        assert!(!app.shows_activity(), "and stops being busy when it finishes");
    }

    /// Activity is process-wide, not active-session-wide: a background
    /// session running while the focused one sits idle still animates its
    /// Projects-pane row, so the gate and the tab title must both fire.
    #[test]
    fn animation_gate_counts_a_non_active_session() {
        use crate::app::session::{SessionLifecycleState, UiSession};

        let mut app = App::test_default();
        assert!(!app.shows_activity(), "focused session idle, nothing else running");

        let other = forge_workspace::SessionKey::from_session_id("other-project");
        let mut session = UiSession::new(other.clone());
        session.lifecycle_state = SessionLifecycleState::Running;
        app.sessions.insert(other.clone(), session);

        assert!(
            app.shows_activity(),
            "another session's turn is still something happening; got status {:?}",
            app.status,
        );
        assert!(
            matches!(app.status, AppStatus::Ready),
            "and it holds without the focused session's status moving; got {:?}",
            app.status,
        );
    }

    /// The gate never lands above either pinned step, at every accepted
    /// fps rather than just the default. The tab-title pulse is written
    /// outside the repaint gate, so nothing the gate paints depends on
    /// the pulse step; that half is held deliberately - see
    /// busytools/forge#587.
    #[test]
    fn repaint_gate_stays_at_or_under_every_pinned_step() {
        for fps in [30, 60, 90, 120, 240] {
            let repaint = RepaintCadence::from_fps(fps).frame_interval();
            assert!(
                repaint <= PULSE_INTERVAL,
                "fps={fps}: a {repaint:?} gate is coarser than the pulse step",
            );
        }
        assert!(
            SPINNER_FRAME_INTERVAL_REDUCED
                <= Duration::from_millis(crate::ui::spinner::REDUCED_FLOOR_MS),
            "reduced-motion repaint must still cover the reduced glyph floor",
        );
    }

    /// Several loop wakes span one animation step, so the gate must let
    /// one repaint through per step rather than one per wake. Reading
    /// the step off the epoch (rather than accumulating per advance)
    /// is what keeps it phase-locked to the rendered glyphs. Stated as a
    /// property of the live cadence, so it survives a default change.
    #[test]
    fn spinner_repaint_step_advances_once_per_interval() {
        let interval = RepaintCadence::default().frame_interval();
        let tick = loop_tick(interval);
        let span = interval * 4;
        let wakes = u32::try_from(span.as_micros() / tick.as_micros()).expect("bounded by cadence");
        let steps: Vec<u128> =
            (0..=wakes).map(|wake| spinner_animation_step(tick * wake, interval)).collect();

        let changes = steps.windows(2).filter(|w| w[0] != w[1]).count();
        let last = usize::try_from(*steps.last().expect("at least one wake")).expect("small");
        assert_eq!(steps[0], 0);
        assert_eq!(changes, last, "the step must advance once per interval crossed, never twice");
        assert!(
            steps.len() > changes * 2,
            "{} wakes over {changes} steps - the gate is not suppressing repaints between steps",
            steps.len(),
        );
    }

    /// A frame interval that isn't a whole number of milliseconds has to
    /// survive the gate's division, or `fps = 120` silently becomes the
    /// 8ms/125fps that integer-ms truncation would give.
    #[test]
    fn repaint_step_honours_a_sub_millisecond_interval() {
        let interval = RepaintCadence::from_fps(120).frame_interval();
        assert_eq!(spinner_animation_step(Duration::from_micros(8332), interval), 0);
        assert_eq!(spinner_animation_step(Duration::from_micros(8334), interval), 1);
        assert_eq!(
            spinner_animation_step(Duration::from_millis(1000), interval),
            120,
            "one second of a 120fps gate is 120 repaint steps",
        );
    }

    /// The loop's wake tick has to be fine enough to land on a frame
    /// boundary. `loop_tick` only ever tightens it, so the tick tracks
    /// whichever of the two is finer - at 120 and below that is still
    /// the 4ms constant.
    #[test]
    fn loop_tick_tightens_only_when_the_frame_interval_demands_it() {
        let tick_for = |fps| loop_tick(RepaintCadence::from_fps(fps).frame_interval());
        assert!(
            loop_tick(RepaintCadence::default().frame_interval()) <= LOOP_TICK,
            "the tick may tighten with the default cadence but must never loosen",
        );
        assert_eq!(tick_for(30), LOOP_TICK);
        assert_eq!(tick_for(60), LOOP_TICK);
        assert_eq!(tick_for(120), LOOP_TICK, "8.3ms still has room for two 4ms wakes");
        assert_eq!(tick_for(240), Duration::from_micros(2083));
        for fps in [30, 60, 90, 120, 240] {
            let interval = RepaintCadence::from_fps(fps).frame_interval();
            assert!(
                loop_tick(interval) * 2 <= interval,
                "fps={fps}: a tick coarser than half the frame interval cannot honour it",
            );
        }
    }

    /// `[ui] fps` must not reach the pulse counter. Its two consumers
    /// are a two-glyph tab-title alternation and a four-step thumb
    /// cycle, both tuned to the pinned step - driven off the repaint
    /// interval instead, 120fps blinks them at 12Hz and 30Hz.
    #[test]
    fn a_high_frame_rate_does_not_speed_up_the_pulse_counter() {
        let mut app = App::test_default();
        app.repaint_cadence = RepaintCadence::from_fps(240);
        let base = Instant::now();

        advance_spinner_frame(&mut app, base);
        assert_eq!(app.spinner_frame, 1);
        // A repaint-driven counter would have stepped twice by now.
        advance_spinner_frame(&mut app, base + Duration::from_millis(10));
        assert_eq!(app.spinner_frame, 1, "the pulse must not follow [ui] fps");
        advance_spinner_frame(&mut app, base + PULSE_INTERVAL);
        assert_eq!(app.spinner_frame, 2, "it steps on its own pinned interval");
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
