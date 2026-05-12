use super::dialog::DialogState;
use super::paste_burst::CharAction;
use super::{
    App, AppStatus, CancelOrigin, FocusOwner, FocusTarget, HelpView, InvalidationLevel, ModeInfo,
    ModeState,
};
#[cfg(not(test))]
use crate::app::SystemSeverity;
use crate::app::inline_interactions::{
    clear_inline_interaction_focus, focus_next_inline_interaction, handle_inline_interaction_key,
};
use crate::app::selection::{clear_selection, selection_text_from_rendered_lines};
use crate::app::state::AutocompleteKind;
use crate::app::{mention, questions, slash, subagent};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
#[cfg(test)]
use std::cell::Cell;
use std::sync::Arc;
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
#[cfg(target_os = "macos")]
pub(crate) const CMD_MOD: KeyModifiers = KeyModifiers::SUPER;
#[cfg(not(target_os = "macos"))]
pub(crate) const CMD_MOD: KeyModifiers = KeyModifiers::CONTROL;

#[cfg(target_os = "macos")]
pub(crate) const WORD_NAV_MOD: KeyModifiers = KeyModifiers::ALT;
#[cfg(not(target_os = "macos"))]
pub(crate) const WORD_NAV_MOD: KeyModifiers = KeyModifiers::CONTROL;

// Modifier that must NOT be set alongside WORD_NAV_MOD. On macOS, ALT is
// the word-nav modifier so the disqualifier is empty (any modifier mix
// containing ALT is fine). On Linux/Windows, CTRL+ALT must NOT trigger
// word-nav (reserved for the host's compose / hotkey paths).
#[cfg(target_os = "macos")]
pub(crate) const WORD_NAV_MOD_EXCLUDED: KeyModifiers = KeyModifiers::empty();
#[cfg(not(target_os = "macos"))]
pub(crate) const WORD_NAV_MOD_EXCLUDED: KeyModifiers = KeyModifiers::ALT;

fn is_ctrl_shortcut(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT)
}

fn is_cmd_shortcut(modifiers: KeyModifiers) -> bool {
    modifiers.contains(CMD_MOD) && !modifiers.contains(KeyModifiers::ALT)
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

fn is_permission_ctrl_shortcut(key: KeyEvent) -> bool {
    is_ctrl_char_shortcut(key, 'y')
        || is_ctrl_char_shortcut(key, 'a')
        || is_ctrl_char_shortcut(key, 'n')
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
    let selection = app.selection?;
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

    if matches!(app.status, AppStatus::Connecting | AppStatus::CommandPending | AppStatus::Error)
        || app.is_compacting()
    {
        return handle_blocked_input_shortcuts(app, key);
    }

    sync_help_focus(app);

    if handle_global_shortcuts(app, key) {
        return true;
    }

    match app.focus_owner() {
        FocusOwner::Mention => handle_autocomplete_key(app, key),
        FocusOwner::Help => handle_help_key(app, key),
        FocusOwner::Permission => {
            if should_reclaim_input_focus_before_inline_interaction(app, key) {
                reclaim_input_from_inline_prompt_if_needed(app);
                handle_normal_key(app, key)
            } else if handle_inline_interaction_key(app, key) {
                true
            } else {
                handle_normal_key(app, key)
            }
        }
        FocusOwner::Input | FocusOwner::TodoList => handle_normal_key(app, key),
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
    // Permission quick shortcuts are global when permissions are pending.
    if !app.pending_interaction_ids().is_empty() && is_permission_ctrl_shortcut(key) {
        return handle_inline_interaction_key(app, key);
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('t'), m) if m == KeyModifiers::CONTROL => {
            toggle_todo_panel_focus(app);
            true
        }
        (KeyCode::Char('x'), m) if m == KeyModifiers::CONTROL => {
            toggle_all_tool_calls(app);
            true
        }
        (KeyCode::Char('b'), m) if m == KeyModifiers::CONTROL => {
            toggle_projects_pane(app);
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
    }

    sync_help_focus(app);
    changed
}

fn should_ignore_key_during_paste(app: &mut App, key: KeyEvent) -> bool {
    if app.pending_submit.is_some() && is_editing_like_key(key) {
        app.pending_submit = None;
    }
    !app.pending_paste_text.is_empty() && is_editing_like_key(key)
}

fn is_editing_like_key(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Delete
    )
}

fn should_reclaim_input_focus_before_inline_interaction(app: &App, key: KeyEvent) -> bool {
    let question_notes_editing = questions::focused_question_is_editing_notes(app);
    match key.code {
        KeyCode::Backspace | KeyCode::Delete => !question_notes_editing,
        KeyCode::Char(_) if is_printable_text_modifiers(key.modifiers) => !question_notes_editing,
        _ => false,
    }
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
    // open — Esc closes it before any other Esc semantics fire.
    if app.projects_pane_overlay_open {
        app.projects_pane_overlay_open = false;
        app.invalidate_layout(InvalidationLevel::Global);
        app.needs_redraw = true;
        return true;
    }
    app.pending_submit = None;
    // Clear any pending image attachments on Escape.
    if !app.pending_images.is_empty() {
        app.pending_images.clear();
        app.needs_redraw = true;
    }
    if app.focus_owner() == FocusOwner::TodoList {
        app.release_focus_target(FocusTarget::TodoList);
        return true;
    }
    if matches!(app.status, AppStatus::Thinking | AppStatus::Running)
        && let Err(message) = super::input_submit::request_cancel(app, CancelOrigin::Manual)
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
    if !matches!(key.code, KeyCode::Enter) || app.focus_owner() == FocusOwner::TodoList {
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
        app.pending_submit = Some(app.input().snapshot());
        tracing::debug!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "deferred_submit_armed",
            message = "deferred submit snapshot armed",
            outcome = "start",
        );
        return false;
    }
    app.pending_submit = None;
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "explicit_newline_inserted",
        message = "explicit newline inserted instead of submit",
        outcome = "success",
    );
    app.input_mut().textarea_insert_newline()
}

fn handle_history_key(app: &mut App, key: KeyEvent) -> bool {
    if app.focus_owner() == FocusOwner::TodoList {
        return false;
    }
    match (key.code, key.modifiers) {
        // macOS: Cmd+Z undo. Linux/Windows: Ctrl+Z undo.
        (KeyCode::Char('z'), m) if m == CMD_MOD => app.input_mut().textarea_undo(),
        // macOS: Cmd+Shift+Z redo. Ghostty / kitty enhanced keyboard
        // reports this as lowercase 'z' with SUPER | SHIFT bits set;
        // some terminals report uppercase 'Z' with SUPER. Match both.
        #[cfg(target_os = "macos")]
        (KeyCode::Char('z'), m) if m == CMD_MOD | KeyModifiers::SHIFT => {
            app.input_mut().textarea_redo()
        }
        #[cfg(target_os = "macos")]
        (KeyCode::Char('Z'), m) if m == CMD_MOD => app.input_mut().textarea_redo(),
        // Linux/Windows: Ctrl+Y redo.
        #[cfg(not(target_os = "macos"))]
        (KeyCode::Char('y'), m) if m == CMD_MOD => app.input_mut().textarea_redo(),
        _ => false,
    }
}

fn handle_navigation_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        // Word left: Alt+Left on macOS, Ctrl+Left elsewhere.
        (KeyCode::Left, m)
            if app.focus_owner() != FocusOwner::TodoList
                && m.contains(WORD_NAV_MOD)
                && !m.intersects(WORD_NAV_MOD_EXCLUDED) =>
        {
            app.input_mut().textarea_move_word_left()
        }
        // Word right: Alt+Right on macOS, Ctrl+Right elsewhere.
        (KeyCode::Right, m)
            if app.focus_owner() != FocusOwner::TodoList
                && m.contains(WORD_NAV_MOD)
                && !m.intersects(WORD_NAV_MOD_EXCLUDED) =>
        {
            app.input_mut().textarea_move_word_right()
        }
        // macOS readline-style fallbacks: many terminals (Ghostty,
        // iTerm2, Terminal.app) send Option+Left as ESC+b and
        // Option+Right as ESC+f rather than Left/Right with ALT.
        // Crossterm decodes those as Char('b')/Char('f') with ALT.
        #[cfg(target_os = "macos")]
        (KeyCode::Char('b'), m)
            if app.focus_owner() != FocusOwner::TodoList && m == KeyModifiers::ALT =>
        {
            app.input_mut().textarea_move_word_left()
        }
        #[cfg(target_os = "macos")]
        (KeyCode::Char('f'), m)
            if app.focus_owner() != FocusOwner::TodoList && m == KeyModifiers::ALT =>
        {
            app.input_mut().textarea_move_word_right()
        }
        (KeyCode::Left, _) if app.focus_owner() != FocusOwner::TodoList => {
            app.input_mut().textarea_move_left()
        }
        (KeyCode::Right, _) if app.focus_owner() != FocusOwner::TodoList => {
            app.input_mut().textarea_move_right()
        }
        (KeyCode::Up, _) if app.focus_owner() == FocusOwner::TodoList => {
            move_todo_selection_up(app);
            true
        }
        (KeyCode::Down, _) if app.focus_owner() == FocusOwner::TodoList => {
            move_todo_selection_down(app);
            true
        }
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
        (KeyCode::Home, _) if app.focus_owner() != FocusOwner::TodoList => {
            app.input_mut().textarea_move_home()
        }
        (KeyCode::End, _) if app.focus_owner() != FocusOwner::TodoList => {
            app.input_mut().textarea_move_end()
        }
        _ => false,
    }
}

fn handle_focus_toggle_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Tab, m)
            if !m.contains(KeyModifiers::SHIFT)
                && !m.contains(KeyModifiers::CONTROL)
                && !m.contains(KeyModifiers::ALT) =>
        {
            if !app.pending_interaction_ids().is_empty() {
                match app.focus_owner() {
                    FocusOwner::Permission => {
                        clear_inline_interaction_focus(app);
                        true
                    }
                    FocusOwner::Input => {
                        focus_next_inline_interaction(app);
                        true
                    }
                    _ => false,
                }
            } else if app.show_todo_panel() && !app.todos().is_empty() {
                if app.focus_owner() == FocusOwner::TodoList {
                    app.release_focus_target(FocusTarget::TodoList);
                } else {
                    app.claim_focus_target(FocusTarget::TodoList);
                }
                true
            } else {
                false
            }
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
    if !matches!(key.code, KeyCode::BackTab) {
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
        .map(|m| ModeInfo { id: m.id.clone(), name: m.name.clone() })
        .collect();

    if let Some(conn) = app.conn()
        && let Some(sid) = app.session_id()
    {
        let mode_id = next_id.clone();
        let conn = Arc::clone(conn);
        tokio::task::spawn_local(async move {
            if let Err(e) = conn.set_mode(sid.to_string(), mode_id) {
                tracing::error!(
                    target: crate::logging::targets::APP_INPUT,
                    event_name = "mode_change_request_failed",
                    message = "failed to request mode change",
                    outcome = "failure",
                    error_message = %e,
                );
            }
        });
    }

    app.set_mode(Some(ModeState {
        current_mode_id: next_id,
        current_mode_name: next_name,
        available_modes: modes,
    }));
    app.invalidate_layout(InvalidationLevel::Global);
    true
}

fn handle_clipboard_paste_key(app: &mut App, key: KeyEvent) -> bool {
    if !is_clipboard_paste_shortcut(key) || app.focus_owner() == FocusOwner::TodoList {
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
                    app.pending_images.push(attachment);
                    // Insert badge text at the cursor position so the user (and
                    // the model) can see where images are relative to text.
                    let idx = app.pending_images.len();
                    let badge = format!("[Image #{idx}]");
                    app.input_mut().insert_str(&badge);
                    app.needs_redraw = true;
                    tracing::debug!(
                        count = app.pending_images.len(),
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

pub(super) fn reclaim_input_from_inline_prompt_if_needed(app: &mut App) {
    if app.focus_owner() == FocusOwner::Permission {
        clear_inline_interaction_focus(app);
    }
}

fn handle_editing_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        // Delete word backward: Alt+Backspace on macOS, Ctrl+Backspace elsewhere.
        (KeyCode::Backspace, m)
            if app.focus_owner() != FocusOwner::TodoList
                && m.contains(WORD_NAV_MOD)
                && !m.intersects(WORD_NAV_MOD_EXCLUDED) =>
        {
            reclaim_input_from_inline_prompt_if_needed(app);
            if try_delete_image_badge(app, "before") {
                return true;
            }
            app.input_mut().textarea_delete_word_before()
        }
        // Delete word forward: Alt+Delete on macOS, Ctrl+Delete elsewhere.
        (KeyCode::Delete, m)
            if app.focus_owner() != FocusOwner::TodoList
                && m.contains(WORD_NAV_MOD)
                && !m.intersects(WORD_NAV_MOD_EXCLUDED) =>
        {
            reclaim_input_from_inline_prompt_if_needed(app);
            if try_delete_image_badge(app, "after") {
                return true;
            }
            app.input_mut().textarea_delete_word_after()
        }
        (KeyCode::Backspace, _) if app.focus_owner() != FocusOwner::TodoList => {
            reclaim_input_from_inline_prompt_if_needed(app);
            if try_delete_image_badge(app, "before") {
                return true;
            }
            app.input_mut().textarea_delete_char_before()
        }
        (KeyCode::Delete, _) if app.focus_owner() != FocusOwner::TodoList => {
            reclaim_input_from_inline_prompt_if_needed(app);
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
    if array_idx < app.pending_images.len() {
        app.pending_images.remove(array_idx);
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
    if app.focus_owner() == FocusOwner::TodoList {
        app.release_focus_target(FocusTarget::TodoList);
    }
    reclaim_input_from_inline_prompt_if_needed(app);

    let now = Instant::now();
    match app.paste_burst.on_char(c, now) {
        CharAction::Consumed => {
            // Character absorbed into burst buffer. Don't insert.
            return false;
        }
        CharAction::RetroCapture(delete_count) => {
            // Burst confirmation retro-captured already-inserted leading chars.
            for _ in 0..delete_count {
                let _ = app.input_mut().textarea_delete_char_before();
            }
            tracing::debug!(
                target: crate::logging::targets::APP_PASTE,
                event_name = "paste_retro_capture_applied",
                message = "retro-captured leaked characters from a confirmed paste burst",
                outcome = "success",
                delete_count,
            );
            return true;
        }
        CharAction::Passthrough(ch) => {
            // Normal typing or a previously-held char released.
            // If `ch == c`, single normal insert. Otherwise the detector
            // emitted a held char; insert it first, then the current char.
            if ch == c {
                let _ = app.input_mut().textarea_insert_char(c);
            } else {
                let _ = app.input_mut().textarea_insert_char(ch);
                let _ = app.input_mut().textarea_insert_char(c);
            }
        }
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

fn should_sync_autocomplete_after_key(app: &App, key: KeyEvent) -> bool {
    if app.focus_owner() == FocusOwner::TodoList {
        return false;
    }

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
        (KeyCode::Char('z'), m) if m == CMD_MOD => true,
        #[cfg(target_os = "macos")]
        (KeyCode::Char('z'), m) if m == CMD_MOD | KeyModifiers::SHIFT => true,
        #[cfg(target_os = "macos")]
        (KeyCode::Char('Z'), m) if m == CMD_MOD => true,
        #[cfg(not(target_os = "macos"))]
        (KeyCode::Char('y'), m) if m == CMD_MOD => true,
        (KeyCode::Char(_), m) if is_printable_text_modifiers(m) => true,
        _ => false,
    }
}

pub(super) fn toggle_todo_panel_focus(app: &mut App) {
    if app.todos().is_empty() {
        app.set_show_todo_panel(false);
        app.release_focus_target(FocusTarget::TodoList);
        app.set_todo_scroll(0);
        app.set_todo_selected(0);
        return;
    }

    app.set_show_todo_panel(!app.show_todo_panel());
    if app.show_todo_panel() {
        // Start at in-progress todo when available; fallback to first item.
        let next =
            app.todos().iter().position(|t| t.status == super::TodoStatus::InProgress).unwrap_or(0);
        app.set_todo_selected(next);
        app.claim_focus_target(FocusTarget::TodoList);
    } else {
        app.release_focus_target(FocusTarget::TodoList);
    }
}

pub(super) fn move_todo_selection_up(app: &mut App) {
    if app.todos().is_empty() || !app.show_todo_panel() {
        app.release_focus_target(FocusTarget::TodoList);
        return;
    }
    app.set_todo_selected(app.todo_selected().saturating_sub(1));
}

pub(super) fn move_todo_selection_down(app: &mut App) {
    if app.todos().is_empty() || !app.show_todo_panel() {
        app.release_focus_target(FocusTarget::TodoList);
        return;
    }
    let max = app.todos().len().saturating_sub(1);
    if app.todo_selected() < max {
        app.set_todo_selected(app.todo_selected().saturating_add(1));
    }
}

/// Handle keystrokes while mention/slash autocomplete dropdown is active.
pub(super) fn handle_autocomplete_key(app: &mut App, key: KeyEvent) -> bool {
    match app.active_autocomplete_kind() {
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
    if app.is_help_active()
        && app.pending_interaction_ids().is_empty()
        && !app.autocomplete_focus_available()
    {
        app.claim_focus_target(FocusTarget::Help);
    } else {
        app.release_focus_target(FocusTarget::Help);
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

/// Toggle the session-level collapsed preference for non-Execute tool calls.
pub(super) fn toggle_all_tool_calls(app: &mut App) {
    app.tools_collapsed = !app.tools_collapsed;
    app.invalidate_layout(InvalidationLevel::Global);
}

/// Tier-aware Ctrl+B handler.
///
/// At Wide / Medium tiers (terminal width ≥ `MEDIUM_TIER_MIN_WIDTH`)
/// this toggles the inline pane's persisted visibility — same
/// behaviour as Phases 2b-α / 2b-β. At Narrow tier it toggles the
/// transient `projects_pane_overlay_open` flag, opening or closing
/// the full-screen overlay rendered by
/// [`crate::ui::projects_pane::render_overlay`]. The overlay flag
/// is intentionally NOT persisted — each launch starts closed.
///
/// Test-only `App::test_default` paths have no workspace, so the
/// persistence step is skipped silently.
pub(super) fn toggle_projects_pane(app: &mut App) {
    let area_width = app.cached_frame_area.width;
    if area_width < crate::ui::layout::MEDIUM_TIER_MIN_WIDTH {
        app.projects_pane_overlay_open = !app.projects_pane_overlay_open;
    } else {
        app.projects_pane_visible = !app.projects_pane_visible;
        if let Some(workspace) = app.workspace.as_ref() {
            workspace.set_projects_pane_visible(app.projects_pane_visible);
        }
    }
    app.invalidate_layout(InvalidationLevel::Global);
    app.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn queued_paste_still_blocks_overlapping_key_text() {
        let mut app = App::test_default();
        app.pending_paste_text = "clipboard".to_owned();

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
            None,
        ));
        app.bind_active_turn_assistant(0);
        app.rendered_chat_area = Rect::new(0, 0, 20, 6);
        app.rendered_chat_lines = vec!["hello".to_owned()];
        app.selection = Some(SelectionState {
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
        app.selection = Some(SelectionState {
            kind: SelectionKind::Input,
            start: SelectionPoint { row: 0, col: 0 },
            end: SelectionPoint { row: 0, col: 11 },
            dragging: false,
        });

        app.input_mut().set_text("hello world");

        assert_eq!(selection_text_for_copy(&mut app), Some("hello world".to_owned()));
    }

    /// Helper: build a tempdir-backed Workspace + plant it on the app
    /// so the Wide/Medium-tier `toggle_projects_pane` branches have
    /// somewhere to persist the new visibility flag.
    fn with_workspace(app: &mut App) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("forge.toml"),
            r#"
[[projects]]
name = "test-proj"
path = "/tmp/test-proj"
default = true

[[accounts]]
display_name = "Test"
config_dir = "/tmp/test-account"
"#,
        )
        .expect("write forge.toml");
        let runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let workspace = runtime.block_on(async {
            forge_workspace::Workspace::new(dir.path().to_owned()).await.expect("workspace")
        });
        app.workspace = Some(std::sync::Arc::new(workspace));
        dir
    }

    #[test]
    fn ctrl_b_at_wide_tier_flips_visibility_and_persists() {
        let mut app = App::test_default();
        let _guard = with_workspace(&mut app);
        app.cached_frame_area = Rect::new(0, 0, crate::ui::layout::WIDE_TIER_MIN_WIDTH, 40);
        app.projects_pane_visible = true;
        app.projects_pane_overlay_open = false;

        toggle_projects_pane(&mut app);

        assert!(!app.projects_pane_visible, "Wide tier flips persistent visibility");
        assert!(!app.projects_pane_overlay_open, "Wide tier leaves overlay flag alone");
        let workspace = app.workspace.as_ref().expect("workspace");
        assert!(
            !workspace.projects_pane_visible(),
            "new visibility persists through forge-state.toml",
        );
    }

    #[test]
    fn ctrl_b_at_medium_tier_flips_visibility_and_persists() {
        let mut app = App::test_default();
        let _guard = with_workspace(&mut app);
        app.cached_frame_area = Rect::new(0, 0, crate::ui::layout::MEDIUM_TIER_MIN_WIDTH, 40);
        app.projects_pane_visible = false;
        app.projects_pane_overlay_open = false;

        toggle_projects_pane(&mut app);

        assert!(app.projects_pane_visible, "Medium tier flips persistent visibility");
        assert!(!app.projects_pane_overlay_open, "Medium tier leaves overlay flag alone");
        let workspace = app.workspace.as_ref().expect("workspace");
        assert!(workspace.projects_pane_visible());
    }

    #[test]
    fn ctrl_b_at_narrow_tier_toggles_overlay_only() {
        // Narrow tier: width < MEDIUM_TIER_MIN_WIDTH. No workspace
        // needed — the narrow branch never persists.
        let mut app = App::test_default();
        app.cached_frame_area = Rect::new(0, 0, crate::ui::layout::MEDIUM_TIER_MIN_WIDTH - 1, 40);
        let initial_visible = app.projects_pane_visible;
        app.projects_pane_overlay_open = false;

        toggle_projects_pane(&mut app);

        assert!(app.projects_pane_overlay_open, "Narrow tier opens overlay");
        assert_eq!(
            app.projects_pane_visible, initial_visible,
            "Narrow tier must not flip the persistent visibility flag",
        );

        // Second invocation closes it back up — confirms the
        // toggle isn't sticky.
        toggle_projects_pane(&mut app);
        assert!(!app.projects_pane_overlay_open, "second Narrow toggle closes the overlay");
        assert_eq!(
            app.projects_pane_visible, initial_visible,
            "persistent visibility still untouched after a second narrow toggle",
        );
    }
}
