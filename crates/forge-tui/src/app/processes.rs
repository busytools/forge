//! View-model for the Inspector pane's `PROCESSES` section.
//!
//! Walks every assistant message in the active session and surfaces
//! the long-running tool kinds claude exposes to forge:
//!
//! - **Backgrounded Bash** (`Bash` invoked with
//!   `run_in_background: true`) — identified by
//!   [`ToolCallInfo::assistant_auto_backgrounded`].
//! - **Monitor** (`Monitor` streaming-process watcher) — identified
//!   by [`is_monitor_tool_name`].
//! - **Cron** (`CronCreate` recurring / one-shot scheduled prompts)
//!   — identified by [`is_cron_create_tool_name`].
//!
//! Each kind's row payload is built from `ToolCallInfo.raw_input`
//! plus the per-kind metadata accessors (`assistant_auto_backgrounded`,
//! `task_is_backgrounded`, etc.) — no new wire surface, no new
//! domain state. Wire captures (2026-05-14) prove this is sufficient
//! for visible kinds: backgrounded Bash + Monitor + Cron all carry
//! the renderable fields directly on tool input.
//!
//! Crate placement: see CLAUDE.md "Crate placement guide". The
//! walker traverses session-level message state and returns a pure
//! view model; `ui::inspector_pane` consumes the view model and
//! chooses glyph + colour. Keeping `forge-tui::ui` free of domain
//! traversal mirrors the `app::git_diff` pattern.

use std::fmt::Write;

use serde_json::Value;

use super::App;
use crate::agent::model::ToolCallStatus;
use crate::app::MessageBlock;
use crate::app::MessageRole;
use crate::app::state::tool_call_info::{
    ToolCallInfo, is_cron_create_tool_name, is_monitor_tool_name,
};

/// One row in the PROCESSES section, materialised from a single
/// long-running tool call in the active session's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    /// Which long-running tool kind produced this row. Determines
    /// the section's glyph + colour at render time.
    pub kind: ProcessKind,
    /// Short headline shown next to the status glyph. Description
    /// for Bash / Monitor (claude's human-written label like
    /// `"Run unit tests"`); the cron expression for Cron (e.g.
    /// `*/5 * * * *`).
    pub headline: String,
    /// Secondary line carrying the underlying detail — the literal
    /// shell command for Bash / Monitor, or the cron prompt for
    /// Cron. `None` when nothing meaningful applies.
    pub detail: Option<String>,
    /// Trailing metadata line: kind label · status · flags.
    /// Pre-rendered as a single string so the renderer doesn't
    /// need to know per-kind formatting rules.
    pub metadata: String,
    /// Current tool-call status (forge's domain-side status).
    /// Drives the status glyph chosen by the renderer.
    pub status: ToolCallStatus,
}

/// Kind discriminator for a [`ProcessRow`]. Used by the renderer to
/// pick the section's status glyph (`▸` for in-flight Bash /
/// Monitor, `⏰` for scheduled Cron).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    /// `Bash` invoked with `run_in_background: true`.
    BashBackgrounded,
    /// `Monitor` streaming-process watcher.
    Monitor,
    /// `CronCreate` scheduled prompt.
    Cron,
}

/// Collect every long-running tool call observable in the active
/// session's message history into a flat list of [`ProcessRow`].
///
/// Iterates the active session's messages (not just the active
/// turn's, because backgrounded Bash / persistent Monitor / Cron
/// outlive the turn that started them). Each assistant message's
/// `ToolCall` content blocks are inspected against
/// `is_*_tool_name` helpers plus `assistant_auto_backgrounded()`;
/// matching tool calls are projected into rows.
///
/// Returns an empty `Vec` when nothing matches — the renderer uses
/// that to hide the section entirely.
#[must_use]
pub fn collect_active_processes(app: &App) -> Vec<ProcessRow> {
    let mut rows = Vec::new();
    for msg in app.messages().iter().filter(|m| matches!(m.role, MessageRole::Assistant)) {
        for block in &msg.blocks {
            let MessageBlock::ToolCall(tc) = block else { continue };
            if let Some(row) = process_row_for(tc) {
                rows.push(row);
            }
        }
    }
    rows
}

/// Project a single `ToolCallInfo` into a `ProcessRow` if it
/// matches one of the long-running tool kinds we surface.
fn process_row_for(tc: &ToolCallInfo) -> Option<ProcessRow> {
    // Bash backgrounded — wins on the metadata flag, irrespective
    // of how `sdk_tool_name` is capitalised in this CLI version.
    if tc.assistant_auto_backgrounded() {
        return Some(bash_backgrounded_row(tc));
    }
    if is_monitor_tool_name(&tc.sdk_tool_name) {
        return Some(monitor_row(tc));
    }
    if is_cron_create_tool_name(&tc.sdk_tool_name) {
        return Some(cron_row(tc));
    }
    None
}

fn bash_backgrounded_row(tc: &ToolCallInfo) -> ProcessRow {
    let raw_input = tc.raw_input.as_ref();
    let description = read_str_field(raw_input, "description");
    let command = read_str_field(raw_input, "command");

    ProcessRow {
        kind: ProcessKind::BashBackgrounded,
        headline: if description.is_empty() {
            "(no description)".to_owned()
        } else {
            description.to_owned()
        },
        detail: if command.is_empty() { None } else { Some(command.to_owned()) },
        metadata: format!("Bash · {}", format_status(tc.status)),
        status: tc.status,
    }
}

fn monitor_row(tc: &ToolCallInfo) -> ProcessRow {
    let raw_input = tc.raw_input.as_ref();
    let description = read_str_field(raw_input, "description");
    let command = read_str_field(raw_input, "command");
    let persistent = read_bool_field(raw_input, "persistent").unwrap_or(false);
    let timeout_ms = read_u64_field(raw_input, "timeout_ms");

    let mut metadata = format!("Monitor · {}", format_status(tc.status));
    if persistent {
        metadata.push_str(" · persistent");
    } else if let Some(ms) = timeout_ms {
        let secs = ms / 1000;
        // `write!` into a String is infallible; the result discard
        // matches clippy's `format_push_string` lint advice.
        let _ = write!(metadata, " · {secs}s timeout");
    }

    ProcessRow {
        kind: ProcessKind::Monitor,
        headline: if description.is_empty() {
            "(no description)".to_owned()
        } else {
            description.to_owned()
        },
        detail: if command.is_empty() { None } else { Some(command.to_owned()) },
        metadata,
        status: tc.status,
    }
}

fn cron_row(tc: &ToolCallInfo) -> ProcessRow {
    let raw_input = tc.raw_input.as_ref();
    let cron_expr = read_str_field(raw_input, "cron");
    let prompt = read_str_field(raw_input, "prompt");
    // Defaults mirror the CronCreate input schema: recurring defaults
    // to `true`, durable defaults to `false`.
    let recurring = read_bool_field(raw_input, "recurring").unwrap_or(true);
    let durable = read_bool_field(raw_input, "durable").unwrap_or(false);

    let mut metadata = String::from("Cron · ");
    metadata.push_str(if recurring { "recurring" } else { "one-shot" });
    metadata.push_str(if durable { " · durable" } else { " · session-only" });

    ProcessRow {
        kind: ProcessKind::Cron,
        headline: if cron_expr.is_empty() {
            "(unknown schedule)".to_owned()
        } else {
            cron_expr.to_owned()
        },
        detail: if prompt.is_empty() { None } else { Some(prompt.to_owned()) },
        metadata,
        status: tc.status,
    }
}

fn format_status(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "running",
        ToolCallStatus::Completed => "done",
        ToolCallStatus::Failed => "failed",
        ToolCallStatus::Killed => "killed",
    }
}

/// Read a `Value::String` field out of a tool's `raw_input` object,
/// returning `""` when absent. The empty-string sentinel lets
/// callers treat "field missing" and "field present but empty"
/// identically without an extra layer of `Option`.
fn read_str_field<'a>(raw_input: Option<&'a Value>, key: &str) -> &'a str {
    raw_input
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn read_bool_field(raw_input: Option<&Value>, key: &str) -> Option<bool> {
    raw_input.and_then(|v| v.as_object()).and_then(|o| o.get(key)).and_then(Value::as_bool)
}

fn read_u64_field(raw_input: Option<&Value>, key: &str) -> Option<u64> {
    raw_input.and_then(|v| v.as_object()).and_then(|o| o.get(key)).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_status_covers_every_variant() {
        assert_eq!(format_status(ToolCallStatus::Pending), "pending");
        assert_eq!(format_status(ToolCallStatus::InProgress), "running");
        assert_eq!(format_status(ToolCallStatus::Completed), "done");
        assert_eq!(format_status(ToolCallStatus::Failed), "failed");
        assert_eq!(format_status(ToolCallStatus::Killed), "killed");
    }

    #[test]
    fn read_str_field_returns_empty_for_missing() {
        let input = json!({"description": "hello"});
        assert_eq!(read_str_field(Some(&input), "description"), "hello");
        assert_eq!(read_str_field(Some(&input), "missing"), "");
        assert_eq!(read_str_field(None, "anything"), "");
    }

    #[test]
    fn read_bool_field_returns_none_for_missing() {
        let input = json!({"persistent": true, "other": "not_bool"});
        assert_eq!(read_bool_field(Some(&input), "persistent"), Some(true));
        assert_eq!(read_bool_field(Some(&input), "other"), None);
        assert_eq!(read_bool_field(Some(&input), "missing"), None);
    }
}
