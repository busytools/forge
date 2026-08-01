use super::dialog::DialogState;
use super::input::TypedChar;
use super::{
    App, AppStatus, FocusOwner, FocusTarget, HelpView, InvalidationLevel, ModeInfo, ModeState,
};
#[cfg(not(test))]
use crate::app::SystemSeverity;
use crate::app::selection::{clear_selection, selection_text_from_rendered_lines};
use crate::app::state::AutocompleteKind;
use crate::app::{InputState, emoji, mention, slash, subagent};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
#[cfg(test)]
use std::cell::Cell;
use std::time::Instant;

const HELP_TAB_PREV_KEY: KeyCode = KeyCode::Left;
const HELP_TAB_NEXT_KEY: KeyCode = KeyCode::Right;

// Platform-aware modifier conventions. macOS native shortcuts use Cmd
// (SUPER) for app-level actions like Cmd+C / Cmd+V / Cmd+Z, and Option
// (ALT) for word navigation. Linux/Windows fall back to Ctrl for both.
//
// Reaching the app: SUPER only arrives when the terminal speaks the
// kitty enhanced-keyboard protocol (Ghostty, kitty, WezTerm). forge-tui
// negotiates DISAMBIGUATE_ESCAPE_CODES + REPORT_EVENT_TYPES +
// REPORT_ALTERNATE_KEYS at startup so this is the case in our stack.
//
// Cmd-prefixed shortcut detection on macOS accepts BOTH SUPER and
// CONTROL - Termux/SSH sessions to the Mac Studio cannot send SUPER
// (Android sends Ctrl), so requiring SUPER would lock SSH'd users
// out of Cmd+C / Cmd+V / Cmd+Z. Treating Ctrl as an equivalent Cmd
// for these app shortcuts costs nothing local (Cmd still works) and
// makes the remote case work too. See `is_cmd_shortcut`.
#[cfg(target_os = "macos")]
pub(crate) const CMD_MOD: KeyModifiers = KeyModifiers::SUPER;
#[cfg(not(target_os = "macos"))]
pub(crate) const CMD_MOD: KeyModifiers = KeyModifiers::CONTROL;

// Word navigation modifier - `Alt` on every platform now. macOS has
// always used Alt (Option+Arrow); Linux/Windows used to use Ctrl+Arrow
// but that conflicts with the side-pane toggle bindings below (Ctrl+
// Left = Projects pane, Ctrl+Right = Inspector pane on non-macOS,
// mirroring the macOS Cmd+Left/Right pair). Moving non-Mac word-nav
// to Alt+Arrow unifies the muscle-memory mapping across platforms and
// makes Ctrl+Arrow available as the pane-toggle binding on Termux
// (Android), Linux desktops, and Windows.
pub(crate) const WORD_NAV_MOD: KeyModifiers = KeyModifiers::ALT;

// Modifier that must NOT be set alongside WORD_NAV_MOD. Empty - any
// modifier mix containing ALT counts as word nav.
pub(crate) const WORD_NAV_MOD_EXCLUDED: KeyModifiers = KeyModifiers::empty();

fn is_ctrl_shortcut(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT)
}

fn is_cmd_shortcut(modifiers: KeyModifiers) -> bool {
    if modifiers.contains(KeyModifiers::ALT) {
        return false;
    }
    if modifiers.contains(CMD_MOD) {
        return true;
    }
    // macOS: accept Ctrl too so SSH/Termux clients (which can't send
    // SUPER) still get Cmd+C / Cmd+V / Cmd+Z. Off macOS this is a
    // no-op because CMD_MOD already equals CONTROL.
    #[cfg(target_os = "macos")]
    {
        modifiers.contains(KeyModifiers::CONTROL)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn ctrl_char(expected: char) -> Option<char> {
    let upper = expected.to_ascii_uppercase();
    if !upper.is_ascii_alphabetic() {
        return None;
    }
    Some(char::from((upper as u8) & 0x1f))
}

pub(super) fn is_ctrl_char_shortcut(key: KeyEvent, expected: char) -> bool {
    match key.code {
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected) => is_ctrl_shortcut(key.modifiers),
        KeyCode::Char(c) if Some(c) == ctrl_char(expected) => {
            !key.modifiers.contains(KeyModifiers::ALT)
        }
        _ => false,
    }
}

/// Cmd-prefixed shortcut: SUPER on macOS, CONTROL elsewhere. On Linux/
/// Windows this collapses to the same predicate as `is_ctrl_char_shortcut`,
/// so callers that accept both Cmd+X and Ctrl+X (e.g. clipboard copy/paste)
/// see no behaviour change off macOS.
pub(super) fn is_cmd_char_shortcut(key: KeyEvent, expected: char) -> bool {
    match key.code {
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected) => is_cmd_shortcut(key.modifiers),
        _ => false,
    }
}

fn handle_always_allowed_shortcuts(app: &mut App, key: KeyEvent) -> bool {
    if is_ctrl_char_shortcut(key, 'q') {
        app.should_quit = true;
        return true;
    }
    // Copy: Cmd+C on macOS, Ctrl+C on Linux/Windows. Both variants are
    // accepted everywhere so muscle memory keeps working across OSes.
    // Ctrl+C with no selection still acts as quit (unchanged); Cmd+C with
    // no selection is a no-op (Mac convention).
    if is_cmd_char_shortcut(key, 'c') || is_ctrl_char_shortcut(key, 'c') {
        match copy_selection_to_clipboard(app) {
            ClipboardCopyResult::Copied => {
                clear_selection(app);
                return true;
            }
            ClipboardCopyResult::Failed => {
                return true;
            }
            ClipboardCopyResult::NoText => {}
        }
        if is_ctrl_char_shortcut(key, 'c') {
            app.should_quit = true;
            return true;
        }
        return false;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardCopyResult {
    Copied,
    Failed,
    NoText,
}

fn copy_selection_to_clipboard(app: &mut App) -> ClipboardCopyResult {
    let Some(selected_text) = selection_text_for_copy(app) else {
        return ClipboardCopyResult::NoText;
    };

    write_text_to_clipboard(selected_text)
}

fn write_text_to_clipboard(selected_text: String) -> ClipboardCopyResult {
    #[cfg(test)]
    {
        match TEST_CLIPBOARD_MODE.with(Cell::get) {
            TestClipboardMode::Succeed => return ClipboardCopyResult::Copied,
            TestClipboardMode::Fail => return ClipboardCopyResult::Failed,
            TestClipboardMode::System => {}
        }
    }

    let selected_chars = selected_text.chars().count();
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        tracing::warn!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "clipboard_access_failed",
            message = "failed to access the clipboard while copying selection",
            outcome = "failure",
            selected_chars,
        );
        return ClipboardCopyResult::Failed;
    };

    if clipboard.set_text(selected_text).is_ok() {
        ClipboardCopyResult::Copied
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "clipboard_write_failed",
            message = "failed to write selection text to the clipboard",
            outcome = "failure",
            selected_chars,
        );
        ClipboardCopyResult::Failed
    }
}

fn selection_text_for_copy(app: &mut App) -> Option<String> {
    let selection = app.selection().copied()?;
    crate::ui::refresh_selection_snapshot(app);
    let lines = match selection.kind {
        super::SelectionKind::Chat => &app.rendered_chat_lines,
        super::SelectionKind::Input => &app.rendered_input_lines,
    };
    let selected_text = selection_text_from_rendered_lines(lines, selection);
    (!selected_text.is_empty()).then_some(selected_text)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestClipboardMode {
    System,
    Succeed,
    Fail,
}

#[cfg(test)]
thread_local! {
    static TEST_CLIPBOARD_MODE: Cell<TestClipboardMode> = const { Cell::new(TestClipboardMode::System) };
}

#[cfg(test)]
pub(crate) struct TestClipboardGuard {
    previous: TestClipboardMode,
}

#[cfg(test)]
impl Drop for TestClipboardGuard {
    fn drop(&mut self) {
        TEST_CLIPBOARD_MODE.with(|mode| mode.set(self.previous));
    }
}

#[cfg(test)]
pub(crate) fn override_test_clipboard(mode: TestClipboardMode) -> TestClipboardGuard {
    let previous = TEST_CLIPBOARD_MODE.with(|current| {
        let previous = current.get();
        current.set(mode);
        previous
    });
    TestClipboardGuard { previous }
}

pub(super) fn dispatch_key_by_focus(app: &mut App, key: KeyEvent) -> bool {
    if handle_always_allowed_shortcuts(app, key) {
        return true;
    }

    // The `/spinner` picker overlay is modal: while open it captures
    // every navigation/commit/cancel key, over chat AND launchpad.
    if app.spinner_picker.is_some() {
        return crate::app::spinner_picker::handle_key(app, key);
    }

    // The `/account` picker overlay is modal too.
    if app.account_picker.is_some() {
        return crate::app::account_picker::handle_key(app, key);
    }

    // Launchpad has its own keymap and intentionally swallows every
    // other key (including Ctrl+B / Ctrl+E and printable input) so
    // nothing leaks into the chat input or pane toggles while the
    // picker is the active view. `Ctrl+Q` is handled by the
    // always-allowed shortcuts above so the user can still quit.
    if app.active_view == crate::app::ActiveView::Launchpad {
        return crate::app::launchpad::handle_key(app, key);
    }

    if matches!(app.status, AppStatus::Connecting | AppStatus::CommandPending | AppStatus::Error)
        || app.is_compacting()
    {
        return handle_blocked_input_shortcuts(app, key);
    }

    sync_help_focus(app);

    if handle_global_shortcuts(app, key) {
        return true;
    }

    // PROMPT MODE: when the active session has a prompt at the head
    // of its queue, all keys route to the prompt dispatcher.
    if app.active_session().is_some_and(|s| !s.prompt_queue.is_empty()) {
        return crate::app::prompt::dispatch_key(app, key);
    }

    match app.focus_owner() {
        FocusOwner::Mention | FocusOwner::Emoji => handle_autocomplete_key(app, key),
        FocusOwner::Help => handle_help_key(app, key),
        FocusOwner::Input => handle_normal_key(app, key),
    }
}

/// During blocked-input states (Connecting, `CommandPending`, Error), keep input disabled and only allow
/// navigation/help shortcuts.
fn handle_blocked_input_shortcuts(app: &mut App, key: KeyEvent) -> bool {
    if is_ctrl_char_shortcut(key, 'l') {
        app.force_redraw = true;
        sync_help_focus(app);
        return true;
    }

    let changed = match (key.code, key.modifiers) {
        (KeyCode::Char('?'), m) if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            if app.is_help_active() {
                app.help_open = false;
                app.input_mut().clear();
            } else {
                app.help_open = true;
                app.input_mut().set_text("?");
            }
            true
        }
        (HELP_TAB_PREV_KEY, m) if m == KeyModifiers::NONE && app.is_help_active() => {
            set_help_view(app, prev_help_view(app.help_view));
            true
        }
        (HELP_TAB_NEXT_KEY, m) if m == KeyModifiers::NONE && app.is_help_active() => {
            set_help_view(app, next_help_view(app.help_view));
            true
        }
        (KeyCode::Up, m) if m == KeyModifiers::NONE || m == KeyModifiers::CONTROL => {
            app.active_viewport_mut().scroll_up(1);
            true
        }
        (KeyCode::Down, m) if m == KeyModifiers::NONE || m == KeyModifiers::CONTROL => {
            app.active_viewport_mut().scroll_down(1);
            true
        }
        _ => false,
    };

    sync_help_focus(app);
    changed
}

/// Handle shortcuts that should work regardless of current focus owner.
fn handle_global_shortcuts(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        // Toggle all tool calls - Cmd+X on macOS, Ctrl+X elsewhere
        // via CMD_MOD. Same platform-modifier convention as the
        // pane-toggle arrows above.
        (KeyCode::Char('x'), m) if is_cmd_shortcut(m) => {
            toggle_all_tool_calls(app);
            true
        }
        // Pane toggles - Cmd+Left / Cmd+Right on macOS, Ctrl+Left /
        // Ctrl+Right elsewhere. One binding per pane, no Ctrl+B /
        // Ctrl+E alias. Cmd+B is the platform's "bold" muscle memory
        // and Ctrl+B is tmux's default prefix on Linux, so the
        // arrow-based bindings avoid both conflicts. Word-nav lives
        // on Alt+Arrow on every platform now (see WORD_NAV_MOD) so
        // Ctrl+Arrow is free on non-macOS.
        //
        // Use the permissive `is_cmd_shortcut` / `is_ctrl_shortcut`
        // predicates rather than `m == SUPER` / `m == CONTROL`.
        // Kitty-keyboard-protocol terminals (Ghostty et al.) can
        // attach extra modifier bits (HYPER, META, NUM_LOCK,
        // CAPS_LOCK) to arrow events; a strict equality match drops
        // those events on the floor. Cmd+Left without Cmd+Char
        // suffered from this because the strict match required
        // `KeyModifiers::SUPER` exactly. The looser predicates just
        // require the platform modifier to be set and ALT (the
        // word-nav modifier) to be unset, so the binding survives
        // protocol-level bit drift.
        #[cfg(target_os = "macos")]
        (KeyCode::Left, m) if is_cmd_shortcut(m) => {
            toggle_projects_pane(app);
            true
        }
        #[cfg(target_os = "macos")]
        (KeyCode::Right, m) if is_cmd_shortcut(m) => {
            toggle_inspector_pane(app);
            true
        }
        #[cfg(not(target_os = "macos"))]
        (KeyCode::Left, m) if is_ctrl_shortcut(m) => {
            toggle_projects_pane(app);
            true
        }
        #[cfg(not(target_os = "macos"))]
        (KeyCode::Right, m) if is_ctrl_shortcut(m) => {
            toggle_inspector_pane(app);
            true
        }
        (KeyCode::Char('l'), m) if m == KeyModifiers::CONTROL => {
            app.force_redraw = true;
            true
        }
        (KeyCode::Up, m) if m == KeyModifiers::CONTROL => {
            app.active_viewport_mut().scroll_up(1);
            true
        }
        (KeyCode::Down, m) if m == KeyModifiers::CONTROL => {
            app.active_viewport_mut().scroll_down(1);
            true
        }
        _ => false,
    }
}

#[inline]
pub(super) fn is_printable_text_modifiers(modifiers: KeyModifiers) -> bool {
    let ctrl_alt =
        modifiers.contains(KeyModifiers::CONTROL) && modifiers.contains(KeyModifiers::ALT);
    !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) || ctrl_alt
}

pub(super) fn handle_normal_key(app: &mut App, key: KeyEvent) -> bool {
    sync_help_focus(app);
    let input_version_before = app.input().version;

    if should_ignore_key_during_paste(app, key) {
        return false;
    }

    let changed = handle_normal_key_actions(app, key);

    if app.input().version != input_version_before {
        app.sync_help_open_with_input();
    }

    if app.input().version != input_version_before && should_sync_autocomplete_after_key(app, key) {
        mention::sync_with_cursor(app);
        slash::sync_with_cursor(app);
        subagent::sync_with_cursor(app);
        emoji::sync_with_cursor(app);
    }

    sync_help_focus(app);
    changed
}

/// Whether a key must be swallowed because a paste payload is still
/// queued for this drain cycle. A chunked bracketed paste can be
/// followed by a trailing newline, which would otherwise read as a
/// submit (chat) or a comment save (/diff) instead of pasted text.
pub(super) fn should_ignore_key_during_paste(app: &mut App, key: KeyEvent) -> bool {
    if app.pending_submit().is_some() && is_editing_like_key(key) {
        *app.pending_submit_mut() = None;
    }
    !app.pending_paste_text().is_empty() && is_editing_like_key(key)
}

fn is_editing_like_key(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Delete
    )
}

fn handle_normal_key_actions(app: &mut App, key: KeyEvent) -> bool {
    if handle_turn_control_key(app, key) {
        return true;
    }
    if handle_submit_key(app, key) {
        return true;
    }
    if handle_history_key(app, key) {
        return true;
    }
    if handle_navigation_key(app, key) {
        return true;
    }
    if handle_focus_toggle_key(app, key) {
        return true;
    }
    if handle_prompt_suggestion_key(app, key) {
        return true;
    }
    if handle_mode_cycle_key(app, key) {
        return true;
    }
    if handle_clipboard_paste_key(app, key) {
        return true;
    }
    if handle_editing_key(app, key) {
        return true;
    }
    handle_printable_key(app, key)
}

fn handle_turn_control_key(app: &mut App, key: KeyEvent) -> bool {
    if !matches!(key.code, KeyCode::Esc) {
        return false;
    }
    // Narrow-tier Projects overlay is the most foreground UI when
    // open - Esc closes it before any other Esc semantics fire.
    if app.projects_pane_overlay_open {
        app.projects_pane_overlay_open = false;
        app.invalidate_layout(InvalidationLevel::Global);
        app.needs_redraw = true;
        return true;
    }
    *app.pending_submit_mut() = None;
    // Clear any pending image attachments on Escape.
    if !app.pending_images().is_empty() {
        app.pending_images_mut().clear();
        app.needs_redraw = true;
    }
    if matches!(app.status, AppStatus::Thinking | AppStatus::Running)
        && let Err(message) = super::input_submit::request_cancel(app)
    {
        tracing::error!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "cancel_request_failed",
            message = "failed to send manual cancel request",
            outcome = "failure",
            error_message = %message,
        );
    }
    true
}

fn handle_submit_key(app: &mut App, key: KeyEvent) -> bool {
    if !matches!(key.code, KeyCode::Enter) {
        return false;
    }

    let now = Instant::now();

    // During an active burst or the post-burst suppression window, Enter
    // becomes a newline to keep multi-line pastes grouped.
    if app.paste_burst.on_enter(now) {
        tracing::debug!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "enter_routed_to_paste_buffer",
            message = "enter was routed through the paste buffer",
            outcome = "success",
        );
        return true;
    }

    if !key.modifiers.contains(KeyModifiers::SHIFT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
    {
        *app.pending_submit_mut() = Some(app.input().snapshot());
        tracing::debug!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "deferred_submit_armed",
            message = "deferred submit snapshot armed",
            outcome = "start",
        );
        return false;
    }
    *app.pending_submit_mut() = None;
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "explicit_newline_inserted",
        message = "explicit newline inserted instead of submit",
        outcome = "success",
    );
    app.input_mut().textarea_insert_newline()
}

fn handle_history_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        // macOS: Cmd+Shift+Z (or Ctrl+Shift+Z) redo. Match Shift-bearing
        // forms BEFORE the plain undo arm - kitty enhanced keyboard
        // sends lowercase 'z' with SUPER | SHIFT; some terminals send
        // uppercase 'Z' with SUPER. Either way, the plain `is_cmd_shortcut`
        // predicate is permissive about extra modifier bits, so the
        // undo arm must come AFTER these to avoid swallowing Shift+Z.
        #[cfg(target_os = "macos")]
        (KeyCode::Char('z'), m)
            if m.contains(KeyModifiers::SHIFT)
                && is_cmd_shortcut(m.difference(KeyModifiers::SHIFT)) =>
        {
            app.input_mut().textarea_redo()
        }
        #[cfg(target_os = "macos")]
        (KeyCode::Char('Z'), m) if is_cmd_shortcut(m) => app.input_mut().textarea_redo(),
        // macOS: Cmd+Z undo. Linux/Windows: Ctrl+Z undo.
        (KeyCode::Char('z'), m) if is_cmd_shortcut(m) && !m.contains(KeyModifiers::SHIFT) => {
            app.input_mut().textarea_undo()
        }
        // Linux/Windows: Ctrl+Y redo.
        #[cfg(not(target_os = "macos"))]
        (KeyCode::Char('y'), m) if is_cmd_shortcut(m) => app.input_mut().textarea_redo(),
        _ => false,
    }
}

fn handle_navigation_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        // Word left: Alt+Left on macOS, Ctrl+Left elsewhere.
        (KeyCode::Left, m) if m.contains(WORD_NAV_MOD) && !m.intersects(WORD_NAV_MOD_EXCLUDED) => {
            app.input_mut().textarea_move_word_left()
        }
        // Word right: Alt+Right on macOS, Ctrl+Right elsewhere.
        (KeyCode::Right, m) if m.contains(WORD_NAV_MOD) && !m.intersects(WORD_NAV_MOD_EXCLUDED) => {
            app.input_mut().textarea_move_word_right()
        }
        // macOS readline-style fallbacks: many terminals (Ghostty,
        // iTerm2, Terminal.app) send Option+Left as ESC+b and
        // Option+Right as ESC+f rather than Left/Right with ALT.
        // Crossterm decodes those as Char('b')/Char('f') with ALT.
        #[cfg(target_os = "macos")]
        (KeyCode::Char('b'), m) if m == KeyModifiers::ALT => {
            app.input_mut().textarea_move_word_left()
        }
        #[cfg(target_os = "macos")]
        (KeyCode::Char('f'), m) if m == KeyModifiers::ALT => {
            app.input_mut().textarea_move_word_right()
        }
        (KeyCode::Left, _) => app.input_mut().textarea_move_left(),
        (KeyCode::Right, _) => app.input_mut().textarea_move_right(),
        (KeyCode::Up, _) => {
            if !try_move_input_cursor_up(app) {
                app.active_viewport_mut().scroll_up(1);
            }
            true
        }
        (KeyCode::Down, _) => {
            if !try_move_input_cursor_down(app) {
                app.active_viewport_mut().scroll_down(1);
            }
            true
        }
        (KeyCode::Home, _) => app.input_mut().textarea_move_home(),
        (KeyCode::End, _) => app.input_mut().textarea_move_end(),
        _ => false,
    }
}

fn handle_focus_toggle_key(_app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Tab, m)
            if !m.contains(KeyModifiers::SHIFT)
                && !m.contains(KeyModifiers::CONTROL)
                && !m.contains(KeyModifiers::ALT) =>
        {
            false
        }
        _ => false,
    }
}

fn handle_prompt_suggestion_key(app: &mut App, key: KeyEvent) -> bool {
    if !matches!(key.code, KeyCode::Tab)
        || !key.modifiers.is_empty()
        || app.focus_owner() != FocusOwner::Input
        || !app.input().is_empty()
    {
        return false;
    }

    let Some(suggestion) = app.prompt_suggestion().map(str::to_owned) else {
        return false;
    };
    if suggestion.trim().is_empty() {
        return false;
    }
    app.set_prompt_suggestion(None);
    app.input_mut().set_text(&suggestion);
    app.sync_help_open_with_input();
    true
}

fn handle_mode_cycle_key(app: &mut App, key: KeyEvent) -> bool {
    // Accept both legacy `BackTab` and kitty-keyboard-protocol
    // `Tab + SHIFT` (Ghostty + some xterm builds emit the latter).
    let is_shift_tab = matches!(key.code, KeyCode::BackTab)
        || (matches!(key.code, KeyCode::Tab) && key.modifiers.contains(KeyModifiers::SHIFT));
    if !is_shift_tab {
        return false;
    }
    let Some(mode) = app.mode() else {
        return true;
    };
    if mode.available_modes.len() <= 1 {
        return true;
    }

    let current_idx =
        mode.available_modes.iter().position(|m| m.id == mode.current_mode_id).unwrap_or(0);
    let next_idx = (current_idx + 1) % mode.available_modes.len();
    let next = &mode.available_modes[next_idx];
    let next_id = next.id.clone();
    let next_name = next.name.clone();
    let modes = mode
        .available_modes
        .iter()
        .map(|m| ModeInfo { id: m.id.clone(), name: m.name.clone(), description: None })
        .collect();

    if app.has_active_agent()
        && app.session_id().is_some()
        && let Some(parsed_mode) = forge_primitives::permission::PermissionMode::from_wire(&next_id)
        && let Err(e) =
            app.dispatch_command(|key| forge_workspace::Command::SetMode { key, mode: parsed_mode })
    {
        tracing::error!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "mode_change_request_failed",
            message = "failed to request mode change",
            outcome = "failure",
            error_message = %e,
        );
    }

    app.set_mode(Some(ModeState {
        current_mode_id: next_id,
        current_mode_name: next_name,
        available_modes: modes,
    }));
    app.invalidate_layout(InvalidationLevel::Global);
    // `set_mode` + layout invalidation don't trigger a redraw on their
    // own - without this the chat history doesn't re-render until the
    // next async update, so the mode chip (and any UI surface that
    // reads `app.mode()`) lags behind the keypress.
    app.needs_redraw = true;
    true
}

fn handle_clipboard_paste_key(#[allow(unused_variables)] app: &mut App, key: KeyEvent) -> bool {
    if !is_clipboard_paste_shortcut(key) {
        return false;
    }
    if key.kind != KeyEventKind::Release {
        return false;
    }

    // Skip system clipboard access in tests to avoid flaky failures / segfaults.
    #[cfg(test)]
    {
        false
    }
    #[cfg(not(test))]
    {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            super::events::push_system_message_with_severity(
                app,
                Some(SystemSeverity::Warning),
                "Failed to access the system clipboard.",
            );
            app.active_viewport_mut().engage_auto_scroll();
            app.needs_redraw = true;
            tracing::warn!("clipboard_paste: failed to access system clipboard");
            return true;
        };

        // Try reading an image from the clipboard first.
        if let Ok(img_data) = clipboard.get_image() {
            match super::clipboard_image::encode_clipboard_image(img_data) {
                Ok(attachment) => {
                    app.pending_images_mut().push(attachment);
                    // Insert badge text at the cursor position so the user (and
                    // the model) can see where images are relative to text.
                    let idx = app.pending_images().len();
                    let badge = format!("[Image #{idx}]");
                    app.input_mut().insert_str(&badge);
                    app.needs_redraw = true;
                    tracing::debug!(
                        count = app.pending_images().len(),
                        "clipboard_paste: attached image from clipboard"
                    );
                    return true;
                }
                Err(error) => {
                    super::events::push_system_message_with_severity(
                        app,
                        Some(SystemSeverity::Warning),
                        error.user_message(),
                    );
                    app.active_viewport_mut().engage_auto_scroll();
                    app.needs_redraw = true;
                    tracing::warn!("clipboard_paste: image attachment failed: {error:?}");
                    return true;
                }
            }
        }

        false
    }
}

pub(super) fn is_clipboard_paste_shortcut(key: KeyEvent) -> bool {
    is_cmd_char_shortcut(key, 'v') || is_ctrl_char_shortcut(key, 'v')
}

fn handle_editing_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        // Delete word backward: Alt+Backspace on macOS, Ctrl+Backspace elsewhere.
        (KeyCode::Backspace, m)
            if m.contains(WORD_NAV_MOD) && !m.intersects(WORD_NAV_MOD_EXCLUDED) =>
        {
            if try_delete_image_badge(app, "before") {
                return true;
            }
            app.input_mut().textarea_delete_word_before()
        }
        // Delete word forward: Alt+Delete on macOS, Ctrl+Delete elsewhere.
        (KeyCode::Delete, m)
            if m.contains(WORD_NAV_MOD) && !m.intersects(WORD_NAV_MOD_EXCLUDED) =>
        {
            if try_delete_image_badge(app, "after") {
                return true;
            }
            app.input_mut().textarea_delete_word_after()
        }
        (KeyCode::Backspace, _) => {
            if try_delete_image_badge(app, "before") {
                return true;
            }
            app.input_mut().textarea_delete_char_before()
        }
        (KeyCode::Delete, _) => {
            if try_delete_image_badge(app, "after") {
                return true;
            }
            app.input_mut().textarea_delete_char_after()
        }
        _ => false,
    }
}

/// If the cursor is inside or adjacent to an `[Image #N]` badge, delete the
/// entire badge, remove the associated image from `pending_images`, and
/// renumber remaining badges. Returns `true` if a badge was deleted.
fn try_delete_image_badge(app: &mut App, direction: &str) -> bool {
    let Some(one_based_idx) = app.input_mut().delete_image_badge(direction) else {
        return false;
    };
    let array_idx = one_based_idx.saturating_sub(1);
    if array_idx < app.pending_images().len() {
        app.pending_images_mut().remove(array_idx);
    }
    app.input_mut().renumber_image_badges();
    app.needs_redraw = true;
    true
}

fn handle_printable_key(app: &mut App, key: KeyEvent) -> bool {
    let (KeyCode::Char(c), m) = (key.code, key.modifiers) else {
        // Non-char key: reset burst state to prevent leakage.
        app.paste_burst.on_non_char_key(Instant::now());
        return false;
    };
    if !is_printable_text_modifiers(m) {
        return false;
    }

    match app.type_char(c, Instant::now()) {
        TypedChar::Buffered => return false,
        TypedChar::RetroCaptured => return true,
        TypedChar::Inserted => {}
    }

    if c == '?' && app.input().text().trim() == "?" {
        app.help_open = true;
    }

    if c == '@' {
        mention::activate(app);
    } else if c == '/' {
        slash::activate(app);
    } else if c == '&' {
        subagent::activate(app);
    } else if c == ':' {
        emoji::activate(app);
    }
    true
}

fn try_move_input_cursor_up(app: &mut App) -> bool {
    let before = (app.input().cursor_row(), app.input().cursor_col());
    let _ = app.input_mut().textarea_move_up();
    (app.input().cursor_row(), app.input().cursor_col()) != before
}

fn try_move_input_cursor_down(app: &mut App) -> bool {
    let before = (app.input().cursor_row(), app.input().cursor_col());
    let _ = app.input_mut().textarea_move_down();
    (app.input().cursor_row(), app.input().cursor_col()) != before
}

fn should_sync_autocomplete_after_key(_app: &App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Enter,
            _,
        ) => true,
        (KeyCode::Char('z'), m) if is_cmd_shortcut(m) => true,
        #[cfg(target_os = "macos")]
        (KeyCode::Char('z'), m)
            if m.contains(KeyModifiers::SHIFT)
                && is_cmd_shortcut(m.difference(KeyModifiers::SHIFT)) =>
        {
            true
        }
        #[cfg(target_os = "macos")]
        (KeyCode::Char('Z'), m) if is_cmd_shortcut(m) => true,
        #[cfg(not(target_os = "macos"))]
        (KeyCode::Char('y'), m) if is_cmd_shortcut(m) => true,
        (KeyCode::Char(_), m) if is_printable_text_modifiers(m) => true,
        _ => false,
    }
}

/// Handle keystrokes while mention/slash autocomplete dropdown is active.
pub(super) fn handle_autocomplete_key(app: &mut App, key: KeyEvent) -> bool {
    match app.active_autocomplete_kind() {
        Some(AutocompleteKind::Emoji) => {
            if handle_emoji_key(app, key) {
                return true;
            }
        }
        Some(AutocompleteKind::Mention) => return handle_mention_key(app, key),
        Some(AutocompleteKind::Slash) => return handle_slash_key(app, key),
        Some(AutocompleteKind::Subagent) => return handle_subagent_key(app, key),
        None => {}
    }
    dispatch_key_by_focus(app, key)
}

fn handle_help_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (HELP_TAB_PREV_KEY, m) if m == KeyModifiers::NONE => {
            set_help_view(app, prev_help_view(app.help_view));
            true
        }
        (HELP_TAB_NEXT_KEY, m) if m == KeyModifiers::NONE => {
            set_help_view(app, next_help_view(app.help_view));
            true
        }
        (KeyCode::Up, m) if m == KeyModifiers::NONE => {
            if matches!(app.help_view, HelpView::SlashCommands | HelpView::Subagents) {
                let count = crate::ui::help::help_item_count(app);
                app.help_dialog.move_up(count, app.help_visible_count);
            }
            true
        }
        (KeyCode::Down, m) if m == KeyModifiers::NONE => {
            if matches!(app.help_view, HelpView::SlashCommands | HelpView::Subagents) {
                let count = crate::ui::help::help_item_count(app);
                app.help_dialog.move_down(count, app.help_visible_count);
            }
            true
        }
        _ => handle_normal_key(app, key),
    }
}

const fn next_help_view(current: HelpView) -> HelpView {
    match current {
        HelpView::Keys => HelpView::SlashCommands,
        HelpView::SlashCommands => HelpView::Subagents,
        HelpView::Subagents => HelpView::Keys,
    }
}

const fn prev_help_view(current: HelpView) -> HelpView {
    match current {
        HelpView::Keys => HelpView::Subagents,
        HelpView::SlashCommands => HelpView::Keys,
        HelpView::Subagents => HelpView::SlashCommands,
    }
}

fn set_help_view(app: &mut App, next: HelpView) {
    if app.help_view != next {
        app.help_view = next;
        app.help_dialog = DialogState::default();
    }
}

fn sync_help_focus(app: &mut App) {
    if app.is_help_active() && !app.autocomplete_focus_available() {
        app.claim_focus_target(FocusTarget::Help);
    } else {
        app.release_focus_target(FocusTarget::Help);
    }
}

/// Handle keystrokes while the `:shortcode` emoji picker is open.
/// Shaped like [`handle_mention_key`], but every edit targets the
/// focused editor so the picker works in the /diff review boxes too.
///
/// Returns `false` for keys the picker does not claim, leaving the
/// caller to apply its own routing - the chat dispatcher in Chat, the
/// overlay's own in /diff. The picker must not fall through itself:
/// doing so would run chat key handling inside the Diff view.
pub(crate) fn handle_emoji_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) => {
            emoji::move_up(app);
            true
        }
        (KeyCode::Down, _) => {
            emoji::move_down(app);
            true
        }
        (KeyCode::Enter | KeyCode::Tab, _) => {
            emoji::confirm_selection(app);
            true
        }
        (KeyCode::Esc, _) => {
            emoji::deactivate(app);
            true
        }
        (KeyCode::Backspace, _) => {
            let changed =
                app.focused_input_mut().is_some_and(InputState::textarea_delete_char_before);
            emoji::update_query(app);
            changed
        }
        // A typed closing `:` on an exact shortcode lands the glyph, so
        // `:tada:` typed straight through works like it does in Slack.
        (KeyCode::Char(':'), m) if is_printable_text_modifiers(m) => {
            if emoji::try_close_shortcode(app) {
                return true;
            }
            let changed =
                app.focused_input_mut().is_some_and(|input| input.textarea_insert_char(':'));
            emoji::update_query(app);
            changed
        }
        (KeyCode::Char(c), m) if is_printable_text_modifiers(m) => {
            let changed =
                app.focused_input_mut().is_some_and(|input| input.textarea_insert_char(c));
            if c.is_whitespace() {
                emoji::deactivate(app);
            } else {
                emoji::update_query(app);
            }
            changed
        }
        // Anything else: close the picker and let the caller route it.
        _ => {
            emoji::deactivate(app);
            false
        }
    }
}

/// Handle keystrokes while the `@` mention autocomplete dropdown is active.
pub(super) fn handle_mention_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) => {
            mention::move_up(app);
            true
        }
        (KeyCode::Down, _) => {
            mention::move_down(app);
            true
        }
        (KeyCode::Enter | KeyCode::Tab, _) => {
            mention::confirm_selection(app);
            true
        }
        (KeyCode::Esc, _) => {
            mention::deactivate(app);
            true
        }
        (KeyCode::Backspace, _) => {
            let changed = app.input_mut().textarea_delete_char_before();
            mention::update_query(app);
            changed
        }
        (KeyCode::Char(c), m) if is_printable_text_modifiers(m) => {
            let changed = app.input_mut().textarea_insert_char(c);
            if c.is_whitespace() {
                mention::deactivate(app);
            } else {
                mention::update_query(app);
            }
            changed
        }
        // Any other key: deactivate mention and forward to normal handling
        _ => {
            mention::deactivate(app);
            dispatch_key_by_focus(app, key)
        }
    }
}

/// Handle keystrokes while slash autocomplete dropdown is active.
fn handle_slash_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) => {
            slash::move_up(app);
            true
        }
        (KeyCode::Down, _) => {
            slash::move_down(app);
            true
        }
        (KeyCode::Enter | KeyCode::Tab, _) => {
            slash::confirm_selection(app);
            true
        }
        (KeyCode::Esc, _) => {
            slash::deactivate(app);
            true
        }
        (KeyCode::Backspace, _) => {
            let changed = app.input_mut().textarea_delete_char_before();
            slash::update_query(app);
            changed
        }
        (KeyCode::Char(c), m) if is_printable_text_modifiers(m) => {
            let changed = app.input_mut().textarea_insert_char(c);
            slash::update_query(app);
            changed
        }
        _ => {
            slash::deactivate(app);
            dispatch_key_by_focus(app, key)
        }
    }
}

/// Handle keystrokes while `&` subagent autocomplete dropdown is active.
fn handle_subagent_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) => {
            subagent::move_up(app);
            true
        }
        (KeyCode::Down, _) => {
            subagent::move_down(app);
            true
        }
        (KeyCode::Enter | KeyCode::Tab, _) => {
            subagent::confirm_selection(app);
            true
        }
        (KeyCode::Esc, _) => {
            subagent::deactivate(app);
            true
        }
        (KeyCode::Backspace, _) => {
            let changed = app.input_mut().textarea_delete_char_before();
            subagent::update_query(app);
            changed
        }
        (KeyCode::Char(c), m) if is_printable_text_modifiers(m) => {
            let changed = app.input_mut().textarea_insert_char(c);
            subagent::update_query(app);
            changed
        }
        _ => {
            subagent::deactivate(app);
            dispatch_key_by_focus(app, key)
        }
    }
}

/// Toggle the session-wide `tools_collapsed` preference and clear
/// every per-item collapse override (per-tool `collapsed_override`,
/// per-peer-text-block `peer_collapsed_override`, per-group
/// `group_collapse_levels`) so any row the user had clicked open or
/// closed resets to its default-render state on the flip. Per-group
/// expand/collapse cycling stays bound to mouse-click on a group
/// summary row (`app::events::mouse::try_toggle_tool_call_at_click`);
/// the keyboard shortcut is the global toggle, always.
pub(super) fn toggle_all_tool_calls(app: &mut App) {
    use crate::app::MessageBlock;
    if let Some(bucket) = app.try_active_bucket_mut() {
        for msg in &mut bucket.messages {
            for block in &mut msg.blocks {
                match block {
                    MessageBlock::ToolCall(tc) => tc.collapsed_override = None,
                    MessageBlock::Text(text) => text.peer_collapsed_override = None,
                    _ => {}
                }
            }
        }
        bucket.group_collapse_levels.clear();
        bucket.messaging_group_collapse_levels.clear();
    }
    app.tools_collapsed = !app.tools_collapsed;
    app.invalidate_layout(InvalidationLevel::Global);
}

/// Tier-aware Ctrl+B handler.
///
/// At Wide / Medium tiers (terminal width ≥ `MEDIUM_TIER_MIN_WIDTH`)
/// this toggles the inline pane's visibility. At Narrow
/// tier it toggles the transient `projects_pane_overlay_open` flag,
/// opening or closing the full-screen overlay rendered by
/// [`crate::ui::projects_pane::render_overlay`].
///
/// Both the inline-pane visibility and the overlay flag are
/// transient (in-memory only). Tier-based defaults at startup pick
/// the initial value; user toggles via this handler stay within the
/// session.
pub(super) fn toggle_projects_pane(app: &mut App) {
    let area_width = app.cached_frame_area.width;
    if area_width < crate::ui::layout::MEDIUM_TIER_MIN_WIDTH {
        // Opening the Projects overlay closes the Inspector overlay
        // (mutually exclusive - both are full-screen).
        if !app.projects_pane_overlay_open {
            app.inspector_pane_overlay_open = false;
        }
        app.projects_pane_overlay_open = !app.projects_pane_overlay_open;
    } else {
        app.projects_pane_visible = !app.projects_pane_visible;
    }
    app.invalidate_layout(InvalidationLevel::Global);
    app.needs_redraw = true;
}

/// Tier-aware Ctrl+E handler - mirror of [`toggle_projects_pane`]
/// for the right Inspector pane. At Wide / Medium tiers flips the
/// in-memory `inspector_pane_visible` flag. At Narrow tier flips
/// the transient `inspector_pane_overlay_open` flag and closes any
/// open Projects overlay (mutually exclusive).
pub(super) fn toggle_inspector_pane(app: &mut App) {
    let area_width = app.cached_frame_area.width;
    if area_width < crate::ui::layout::MEDIUM_TIER_MIN_WIDTH {
        if !app.inspector_pane_overlay_open {
            app.projects_pane_overlay_open = false;
        }
        app.inspector_pane_overlay_open = !app.inspector_pane_overlay_open;
    } else {
        app.inspector_pane_visible = !app.inspector_pane_visible;
    }
    app.invalidate_layout(InvalidationLevel::Global);
    app.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::paste_burst::CharAction;
    use crate::app::{
        ChatMessage, MessageBlock, MessageRole, SelectionKind, SelectionPoint, SelectionState,
        TextBlock,
    };
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::layout::Rect;
    use std::time::{Duration, Instant};

    #[test]
    fn ctrl_shortcut_accepts_standard_ctrl_v_encoding() {
        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert!(is_ctrl_char_shortcut(key, 'v'));
    }

    #[test]
    fn ctrl_shortcut_accepts_raw_control_character_encoding() {
        let key = KeyEvent::new(KeyCode::Char('\u{16}'), KeyModifiers::NONE);
        assert!(is_ctrl_char_shortcut(key, 'v'));
    }

    #[test]
    fn ctrl_shortcut_rejects_raw_control_character_with_alt() {
        let key = KeyEvent::new(KeyCode::Char('\u{16}'), KeyModifiers::ALT);
        assert!(!is_ctrl_char_shortcut(key, 'v'));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shift_z_routes_to_redo_not_undo() {
        // Regression: `is_cmd_shortcut` is permissive about extra
        // modifier bits, so a plain `Char('z') if is_cmd_shortcut(m)`
        // arm placed BEFORE the Shift-bearing redo arm would match
        // Cmd+Shift+Z first and route to undo. The Shift-bearing
        // arms must come first AND the undo arm must exclude Shift.
        // macOS-only: Linux/Windows redo via Ctrl+Y, not Ctrl+Shift+Z,
        // so the regression class doesn't exist there.
        let mut app = App::test_default();
        app.input_mut().set_text("a");
        // Make a delete so we have something to undo, then make
        // another action so we have something to redo.
        app.input_mut().textarea_insert_char('b');
        app.input_mut().textarea_undo();
        let after_undo = app.input().text();

        // Cmd+Shift+Z should redo, not undo again.
        let cmd_shift_z = KeyEvent::new(KeyCode::Char('z'), CMD_MOD | KeyModifiers::SHIFT);
        let consumed = handle_history_key(&mut app, cmd_shift_z);
        assert!(consumed, "Cmd+Shift+Z must be consumed by history handler");
        assert_ne!(
            app.input().text(),
            after_undo,
            "Cmd+Shift+Z must redo (text changes from the post-undo state), not undo again"
        );
    }

    #[test]
    fn ctrl_shift_z_routes_to_redo_on_macos() {
        // Same as above but with Ctrl modifier - exercises the
        // Ctrl-as-Cmd alias on macOS (SSH/Termux clients).
        let mut app = App::test_default();
        app.input_mut().set_text("a");
        app.input_mut().textarea_insert_char('b');
        app.input_mut().textarea_undo();
        let after_undo = app.input().text();

        let ctrl_shift_z =
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        let consumed = handle_history_key(&mut app, ctrl_shift_z);
        // Off macOS, Ctrl+Shift+Z isn't bound (Linux/Windows use
        // Ctrl+Y for redo) - only assert redo behaviour on macOS.
        #[cfg(target_os = "macos")]
        {
            assert!(consumed, "Ctrl+Shift+Z must be consumed by history handler on macOS");
            assert_ne!(
                app.input().text(),
                after_undo,
                "Ctrl+Shift+Z must redo on macOS, not undo again"
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = consumed;
            let _ = after_undo;
        }
    }

    #[test]
    fn queued_paste_still_blocks_overlapping_key_text() {
        let mut app = App::test_default();
        *app.pending_paste_text_mut() = "clipboard".to_owned();

        let blocked = should_ignore_key_during_paste(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert!(blocked);
    }

    #[test]
    fn burst_active_does_not_block_followup_chars() {
        let mut app = App::test_default();
        let t0 = Instant::now();

        assert_eq!(app.paste_burst.on_char('a', t0), CharAction::Passthrough('a'));
        assert_eq!(
            app.paste_burst.on_char('b', t0 + Duration::from_millis(1)),
            CharAction::Consumed
        );
        assert!(app.paste_burst.is_buffering());

        let blocked = should_ignore_key_during_paste(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );
        assert!(!blocked);
    }

    #[test]
    fn selection_text_for_copy_refreshes_chat_snapshot_before_redraw() {
        let mut app = App::test_default();
        app.status = AppStatus::Running;
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete("hello"))],
        ));
        app.bind_active_turn_assistant(0);
        app.rendered_chat_area = Rect::new(0, 0, 20, 6);
        app.rendered_chat_lines = vec!["hello".to_owned()];
        *app.selection_mut() = Some(SelectionState {
            kind: SelectionKind::Chat,
            start: SelectionPoint { row: 0, col: 0 },
            end: SelectionPoint { row: 0, col: 11 },
            dragging: false,
        });

        if let Some(MessageBlock::Text(block)) =
            app.active_messages_mut().get_mut(0).and_then(|message| message.blocks.get_mut(0))
        {
            block.text.push_str(" world");
            block.markdown.append(" world");
            block.cache.invalidate();
        }
        app.invalidate_layout(InvalidationLevel::MessageChanged(0));

        assert!(selection_text_for_copy(&mut app).is_some());
        assert!(app.rendered_chat_lines.iter().any(|line| line.contains("world")));
    }

    #[test]
    fn selection_text_for_copy_refreshes_input_snapshot_before_redraw() {
        let mut app = App::test_default();
        app.input_mut().set_text("hello");
        app.rendered_input_area = Rect::new(0, 0, 20, 4);
        app.rendered_input_lines = vec!["hello".to_owned()];
        *app.selection_mut() = Some(SelectionState {
            kind: SelectionKind::Input,
            start: SelectionPoint { row: 0, col: 0 },
            end: SelectionPoint { row: 0, col: 11 },
            dragging: false,
        });

        app.input_mut().set_text("hello world");

        assert_eq!(selection_text_for_copy(&mut app), Some("hello world".to_owned()));
    }

    #[test]
    fn ctrl_b_at_wide_tier_flips_in_memory_visibility() {
        let mut app = App::test_default();
        app.cached_frame_area = Rect::new(0, 0, crate::ui::layout::WIDE_TIER_MIN_WIDTH, 40);
        app.projects_pane_visible = true;
        app.projects_pane_overlay_open = false;

        toggle_projects_pane(&mut app);

        assert!(!app.projects_pane_visible, "Wide tier flips visibility");
        assert!(!app.projects_pane_overlay_open, "Wide tier leaves overlay flag alone");
    }

    #[test]
    fn ctrl_b_at_medium_tier_flips_in_memory_visibility() {
        let mut app = App::test_default();
        app.cached_frame_area = Rect::new(0, 0, crate::ui::layout::MEDIUM_TIER_MIN_WIDTH, 40);
        app.projects_pane_visible = false;
        app.projects_pane_overlay_open = false;

        toggle_projects_pane(&mut app);

        assert!(app.projects_pane_visible, "Medium tier flips visibility");
        assert!(!app.projects_pane_overlay_open, "Medium tier leaves overlay flag alone");
    }

    #[test]
    fn ctrl_b_at_narrow_tier_toggles_overlay_only() {
        // Narrow tier: width < MEDIUM_TIER_MIN_WIDTH.
        let mut app = App::test_default();
        app.cached_frame_area = Rect::new(0, 0, crate::ui::layout::MEDIUM_TIER_MIN_WIDTH - 1, 40);
        let initial_visible = app.projects_pane_visible;
        app.projects_pane_overlay_open = false;

        toggle_projects_pane(&mut app);

        assert!(app.projects_pane_overlay_open, "Narrow tier opens overlay");
        assert_eq!(
            app.projects_pane_visible, initial_visible,
            "Narrow tier must not flip the inline visibility flag",
        );

        // Second invocation closes it back up - confirms the toggle isn't sticky.
        toggle_projects_pane(&mut app);
        assert!(!app.projects_pane_overlay_open, "second Narrow toggle closes the overlay");
        assert_eq!(
            app.projects_pane_visible, initial_visible,
            "inline visibility still untouched after a second narrow toggle",
        );
    }
}
