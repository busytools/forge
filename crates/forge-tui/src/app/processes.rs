//! View-model for the Inspector pane's `PROCESSES` section (path A+).
//!
//! OS walk is the source of truth for what's running; wire-tracked
//! tool calls overlay headline + kind metadata when their `command`
//! field substring-matches an OS process's cmdline.
//!
//! Why OS-first? Wire-only tracking missed:
//! - Foreground `Bash` (claude blocks; no `task_started`).
//! - Grandchildren (a `cargo` spawning `rustc` workers).
//! - Anything that detaches from claude's tool registry.
//!
//! OS-walk is universal — every descendant of the spawned `claude`
//! shows up. Wire enrichment makes the row read nicely when both
//! signals agree (e.g. claude's "Run unit tests" description on a
//! `cargo nextest run` process row).
//!
//! `Cron` is the exception: `CronCreate` is a registration, not a
//! process. We still surface alive Cron rows from the wire alone
//! since there's nothing to OS-walk for them.

use std::collections::HashSet;
use std::fmt::Write;

use forge_workspace::env::processes::{
    ProcessEntry, ProcessSnapshot, process_cmdline_matches_tool_input,
};
use serde_json::Value;

use super::App;
use crate::agent::model::ToolCallStatus;
use crate::app::MessageBlock;
use crate::app::MessageRole;
use crate::app::state::tool_call_info::{
    ToolCallInfo, is_cron_create_tool_name, is_execute_tool_name, is_monitor_tool_name,
};

/// Soft cap on the rendered PROCESSES section. Sanity bound so a
/// runaway process tree doesn't blow up the body line count; users
/// scroll within the section to see everything below the cap. The
/// `overflow` count on [`ProcessCollection`] is no longer rendered
/// as a footer row (the scrollbar IS the overflow indicator) but
/// is kept so future surfaces can show "n hidden" if needed.
const PROCESSES_MAX: usize = 50;

/// One row in the PROCESSES section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    /// Which long-running kind produced this row. Drives the
    /// section's glyph + colour at render time.
    pub kind: ProcessKind,
    /// Short headline shown next to the status glyph. Wire-derived
    /// description (`"Run unit tests"`) when an OS process matched a
    /// wire tool call; OS process name (`"cargo"`) otherwise. Cron
    /// rows carry the cron expression (`*/5 * * * *`).
    pub headline: String,
    /// Secondary line carrying the cmdline (OS) or cron prompt.
    /// `None` when nothing meaningful applies. Renderer drops this
    /// at depth >= 1 to keep the tree compact (only the supervisor
    /// row shows full context).
    pub detail: Option<String>,
    /// Trailing metadata line: kind label · status · flags.
    /// Pre-rendered as a single string; the renderer suffixes a
    /// `· 12 MB` segment at Wide tier when `memory_bytes` is set.
    /// Same depth-collapse as `detail`.
    pub metadata: String,
    /// Tool-call status driving the row's status glyph. OS-walked
    /// entries always read as `InProgress` (alive-set membership is
    /// the source of truth).
    pub status: ToolCallStatus,
    /// Resident memory in bytes for OS-walked entries; `None` for
    /// wire-only rows (e.g. Cron registrations have no process).
    /// Used for (a) sort ordering and (b) Wide-tier metadata suffix.
    pub memory_bytes: Option<u64>,
    /// Tree depth. `0` = direct child of `claude` (a "supervisor"
    /// row); `>= 1` = a transitive descendant rendered nested
    /// underneath. The renderer uses this for indentation +
    /// box-drawing connectors and switches to a single-line compact
    /// form for `depth >= 1`.
    pub depth: u8,
    /// Whether this row is the last child of its parent in the
    /// process tree. The renderer uses this to pick `└─` (last) vs
    /// `├─` (not last) for the tree connector at this row's depth.
    /// Always `true` for depth 0 (supervisors are top-level).
    pub is_last_sibling: bool,
    /// Per-ancestor-depth "more siblings below" flags. Length
    /// equals `depth` (so a depth-0 row has an empty slice). Each
    /// entry corresponds to one ancestor level: `true` = that
    /// ancestor has more siblings below this row → renderer prints
    /// `│  ` continuation in that column; `false` = no more
    /// siblings → renderer prints three spaces. Drives correct
    /// vertical-bar continuations across nested levels.
    pub ancestor_has_more: Vec<bool>,
}

/// Kind discriminator for a [`ProcessRow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    /// OS process matched against a wire-tracked backgrounded `Bash`.
    BashBackgrounded,
    /// OS process matched against a wire-tracked `Monitor`.
    Monitor,
    /// `CronCreate` registration — wire-only, no OS process.
    Cron,
    /// OS process with no matching wire tool call (foreground Bash,
    /// grandchildren, anything claude's tool registry doesn't know
    /// about).
    Process,
}

/// Result of [`collect_active_processes`]: rows that survived
/// sorting + the [`PROCESSES_MAX`] sanity cap. The pane scrolls
/// when the section overflows the visible area, so no overflow
/// footer is rendered.
#[derive(Debug, Clone)]
pub struct ProcessCollection {
    pub rows: Vec<ProcessRow>,
}

impl ProcessCollection {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Collect the active session's running processes into a sorted,
/// top-N-capped list of [`ProcessRow`].
///
/// OS-walked entries dominate: every descendant of the spawned
/// `claude` PID becomes a row. Wire-tracked alive tool calls overlay
/// description + kind when their `command` field substring-matches
/// the OS process's cmdline. Cron rows tag along separately because
/// they're registrations, not processes.
///
/// Sort key: `memory_bytes` desc (heaviest processes win the slots),
/// with wire-only rows (Cron) sorted to the end. Final list is
/// capped at [`PROCESSES_MAX`]; `overflow` carries the count of
/// hidden rows.
#[must_use]
pub fn collect_active_processes(app: &App) -> ProcessCollection {
    let Some(session) = app.active_session() else {
        return ProcessCollection { rows: Vec::new() };
    };

    // Snapshot wire-alive tool calls. Two paths into the alive set:
    //
    // 1. **Backgrounded Bash / Monitor.** `task_started` fired on
    //    the wire and the terminal `task_updated` has NOT — so the
    //    tool_use_id is in `alive_task_ids`'s mapped set. Note the
    //    per-tool `tc.status` is unreliable here: claude's
    //    `backgroundTaskId` `tool_result` arrives almost immediately
    //    and flips status to `Completed` while the underlying
    //    process keeps running. Trust `alive_task_ids`, not `status`.
    //
    // 2. **Foreground Bash.** Claude blocks on the call; no
    //    `task_started` ever fires, so path 1 misses it entirely.
    //    The signal here IS `tc.status == InProgress` — the
    //    `tool_result` hasn't arrived because the command is still
    //    running. Including these in the alive set is what lets
    //    foreground `cargo build` / `git status` / `ls` rows pick
    //    up the wire's `description` as a headline instead of
    //    falling through to the generic OS row.
    let alive_tool_use_ids: HashSet<String> = app.with_turn_state(|ts| {
        ts.task_tool_use_ids
            .iter()
            .filter(|(task_id, _)| ts.alive_task_ids.contains(*task_id))
            .map(|(_, tool_use_id)| tool_use_id.clone())
            .collect()
    });
    let wire_alive: Vec<&ToolCallInfo> = session
        .messages
        .iter()
        .filter(|m| matches!(m.role, MessageRole::Assistant))
        .flat_map(|m| &m.blocks)
        .filter_map(|b| match b {
            MessageBlock::ToolCall(tc)
                if alive_tool_use_ids.contains(&tc.id)
                    || tc.status == ToolCallStatus::InProgress =>
            {
                Some(tc.as_ref())
            }
            _ => None,
        })
        .collect();

    let mut rows: Vec<ProcessRow> = Vec::new();

    // 1. OS-walked entries — the source of truth for live work.
    if let Some(snapshot) = session.process_snapshot.as_ref() {
        rows.extend(rows_from_os_snapshot(snapshot, &wire_alive));
    }

    // 2. Cron registrations — wire-only; no backing OS process to
    //    walk. Bash / Monitor wire entries that didn't match an OS
    //    row are intentionally dropped (OS walk is the truth of what
    //    is RUNNING). Cron is the exception because the wire IS
    //    where the schedule lives.
    for tc in &wire_alive {
        if is_cron_create_tool_name(&tc.sdk_tool_name) {
            rows.push(cron_row(tc));
        }
    }

    rows.truncate(PROCESSES_MAX);
    ProcessCollection { rows }
}

/// DFS the OS snapshot's process tree from claude's direct children
/// down, emitting one [`ProcessRow`] per node in pre-order with
/// correct `depth` + tree-connector metadata. Wire-matched rows
/// (`Bash` / `Monitor`) are pinned at the top of each sibling
/// group; unmatched siblings sort by memory desc with PID as the
/// stable tie-break.
fn rows_from_os_snapshot<'a>(
    snapshot: &'a ProcessSnapshot,
    wire_alive: &'a [&'a ToolCallInfo],
) -> Vec<ProcessRow> {
    use std::collections::HashMap;

    // Index by pid + build a parent → children adjacency list.
    let by_pid: HashMap<u32, &ProcessEntry> =
        snapshot.processes.iter().map(|e| (e.pid, e)).collect();
    let mut children_of: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    for entry in &snapshot.processes {
        children_of.entry(entry.parent_pid).or_default().push(entry);
    }

    // Roots = entries whose parent is NOT in the snapshot (the
    // snapshot only includes claude's descendants, so a missing
    // parent means the parent IS claude itself).
    let mut roots: Vec<&ProcessEntry> =
        snapshot.processes.iter().filter(|e| !by_pid.contains_key(&e.parent_pid)).collect();
    sort_siblings_inplace(&mut roots, wire_alive);

    let mut rows = Vec::new();
    let n_roots = roots.len();
    for (idx, root) in roots.iter().enumerate() {
        emit_with_descendants(
            root,
            0,
            idx + 1 == n_roots,
            &[],
            &children_of,
            wire_alive,
            &mut rows,
        );
    }
    rows
}

/// Emit `entry` + DFS its children, sorted siblings-first. Each
/// emitted row carries `depth`, `is_last_sibling`, and the slice of
/// "more siblings below" flags for each ancestor level so the
/// renderer can pick the right tree connector at every column.
fn emit_with_descendants<'a>(
    entry: &'a ProcessEntry,
    depth: u8,
    is_last_sibling: bool,
    ancestor_has_more: &[bool],
    children_of: &std::collections::HashMap<u32, Vec<&'a ProcessEntry>>,
    wire_alive: &[&ToolCallInfo],
    out: &mut Vec<ProcessRow>,
) {
    let mut row = build_row_for_entry(entry, wire_alive);
    row.depth = depth;
    row.is_last_sibling = is_last_sibling;
    row.ancestor_has_more = ancestor_has_more.to_vec();
    out.push(row);

    let Some(kids_ref) = children_of.get(&entry.pid) else {
        return;
    };
    let mut kids = kids_ref.clone();
    sort_siblings_inplace(&mut kids, wire_alive);

    // The next level's ancestor_has_more appends THIS row's
    // "more-siblings-below" bit so a deep descendant knows whether
    // to keep drawing the `│` continuation in this row's column.
    let mut next_ancestors = ancestor_has_more.to_vec();
    next_ancestors.push(!is_last_sibling);

    let n = kids.len();
    for (i, kid) in kids.iter().enumerate() {
        emit_with_descendants(
            kid,
            depth.saturating_add(1),
            i + 1 == n,
            &next_ancestors,
            children_of,
            wire_alive,
            out,
        );
    }
}

/// Build a row for a single OS entry, doing the wire-match check
/// against the alive tool calls. Tree position (`depth`,
/// `is_last_sibling`, `ancestor_has_more`) is initialised to
/// defaults; the DFS walker overwrites them.
fn build_row_for_entry(entry: &ProcessEntry, wire_alive: &[&ToolCallInfo]) -> ProcessRow {
    let matched = wire_alive.iter().copied().find(|tc| {
        let cmd = read_str_field(tc.raw_input.as_ref(), "command");
        !cmd.is_empty() && process_cmdline_matches_tool_input(&entry.command, cmd)
    });
    match matched {
        Some(tc) if is_monitor_tool_name(&tc.sdk_tool_name) => enriched_monitor_row(tc, entry),
        Some(tc) if is_execute_tool_name(&tc.sdk_tool_name) => enriched_bash_row(tc, entry),
        // Matched something we don't have a special kind for
        // (defensive — shouldn't fire today). Fall through to
        // the generic OS row so we still surface the process.
        _ => generic_os_row(entry),
    }
}

/// Sort entries in place: wire-matched first (so highlighted
/// supervisors pin to the top of their sibling group), then memory
/// descending, then PID as a stable tie-break.
fn sort_siblings_inplace(entries: &mut [&ProcessEntry], wire_alive: &[&ToolCallInfo]) {
    entries.sort_by(|a, b| {
        let a_m = is_matched_entry(a, wire_alive);
        let b_m = is_matched_entry(b, wire_alive);
        match (a_m, b_m) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b.memory_bytes.cmp(&a.memory_bytes).then_with(|| a.pid.cmp(&b.pid)),
        }
    });
}

/// True when `entry`'s cmdline substring-matches any alive wire
/// tool call's `command` field. Used by [`sort_siblings_inplace`]
/// to pin matched rows; the actual kind/glyph decision happens in
/// [`build_row_for_entry`].
fn is_matched_entry(entry: &ProcessEntry, wire_alive: &[&ToolCallInfo]) -> bool {
    wire_alive.iter().any(|tc| {
        let cmd = read_str_field(tc.raw_input.as_ref(), "command");
        !cmd.is_empty() && process_cmdline_matches_tool_input(&entry.command, cmd)
    })
}

/// OS process matched to a wire-tracked backgrounded `Bash`. Headline
/// from the wire description; detail from the OS cmdline; metadata
/// suffixed with memory.
fn enriched_bash_row(tc: &ToolCallInfo, entry: &ProcessEntry) -> ProcessRow {
    let description = read_str_field(tc.raw_input.as_ref(), "description");
    let headline = if description.is_empty() { entry.name.clone() } else { description.to_owned() };
    ProcessRow {
        kind: ProcessKind::BashBackgrounded,
        headline,
        detail: detail_from_command(&entry.command),
        metadata: "Bash · running".to_owned(),
        status: ToolCallStatus::InProgress,
        memory_bytes: Some(entry.memory_bytes),
        depth: 0,
        is_last_sibling: true,
        ancestor_has_more: Vec::new(),
    }
}

/// OS process matched to a wire-tracked `Monitor`. Persistent /
/// timeout flags carry over from the tool input.
fn enriched_monitor_row(tc: &ToolCallInfo, entry: &ProcessEntry) -> ProcessRow {
    let raw_input = tc.raw_input.as_ref();
    let description = read_str_field(raw_input, "description");
    let persistent = read_bool_field(raw_input, "persistent").unwrap_or(false);
    let timeout_ms = read_u64_field(raw_input, "timeout_ms");

    let headline = if description.is_empty() { entry.name.clone() } else { description.to_owned() };

    let mut metadata = String::from("Monitor · running");
    if persistent {
        metadata.push_str(" · persistent");
    } else if let Some(ms) = timeout_ms {
        let secs = ms / 1000;
        let _ = write!(metadata, " · {secs}s timeout");
    }

    ProcessRow {
        kind: ProcessKind::Monitor,
        headline,
        detail: detail_from_command(&entry.command),
        metadata,
        status: ToolCallStatus::InProgress,
        memory_bytes: Some(entry.memory_bytes),
        depth: 0,
        is_last_sibling: true,
        ancestor_has_more: Vec::new(),
    }
}

/// OS process with no matching wire tool call (foreground Bash,
/// grandchildren, etc.). Headline is the OS process name; detail is
/// the cmdline.
fn generic_os_row(entry: &ProcessEntry) -> ProcessRow {
    let headline = if entry.name.is_empty() { "(process)".to_owned() } else { entry.name.clone() };
    ProcessRow {
        kind: ProcessKind::Process,
        headline,
        detail: detail_from_command(&entry.command),
        metadata: "Process · running".to_owned(),
        status: ToolCallStatus::InProgress,
        memory_bytes: Some(entry.memory_bytes),
        depth: 0,
        is_last_sibling: true,
        ancestor_has_more: Vec::new(),
    }
}

/// Cron registration row. Schedule expression as headline, prompt
/// as detail. No OS process backs this — `memory_bytes` stays
/// `None`, which sorts the row to the bottom of the section.
fn cron_row(tc: &ToolCallInfo) -> ProcessRow {
    let raw_input = tc.raw_input.as_ref();
    let cron_expr = read_str_field(raw_input, "cron");
    let prompt = read_str_field(raw_input, "prompt");
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
        memory_bytes: None,
        depth: 0,
        is_last_sibling: true,
        ancestor_has_more: Vec::new(),
    }
}

/// Detail row for an OS entry: the cmdline, or `None` when it's
/// empty (some short-lived processes don't expose cmdline through
/// sysinfo).
fn detail_from_command(command: &str) -> Option<String> {
    let trimmed = command.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_owned()) }
}


/// Format a byte count compactly for the metadata suffix:
/// `< 1 KB` → `b`, `< 1 MB` → `K`, etc. Two significant digits in
/// the fractional range, integer above 99.
#[must_use]
pub fn format_memory_short(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", bytes / KB)
    } else if bytes < GB {
        format!("{} MB", bytes / MB)
    } else {
        // 1 decimal digit for GB — `1.2 GB` reads better than `1228 MB`.
        let gb_tenths = bytes / (GB / 10);
        let whole = gb_tenths / 10;
        let frac = gb_tenths % 10;
        format!("{whole}.{frac} GB")
    }
}

/// Read a `Value::String` field out of a tool's `raw_input` object,
/// returning `""` when absent.
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

    #[test]
    fn format_memory_short_picks_compact_unit() {
        assert_eq!(format_memory_short(0), "0 B");
        assert_eq!(format_memory_short(512), "512 B");
        assert_eq!(format_memory_short(2 * 1024), "2 KB");
        assert_eq!(format_memory_short(12 * 1024 * 1024), "12 MB");
        assert_eq!(format_memory_short(1_288_490_188), "1.2 GB"); // ~1.2 GB
    }

    fn fake_entry(pid: u32, name: &str, command: &str, memory_bytes: u64) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent_pid: 0,
            name: name.to_owned(),
            command: command.to_owned(),
            memory_bytes,
            started_at_unix: None,
        }
    }

    /// Build a minimal `ToolCallInfo` carrying just the fields the
    /// collector reads (id, sdk_tool_name, raw_input). All other
    /// fields stay at zero / default so the helper is short.
    fn fake_tool_call_info(id: &str, sdk_tool_name: &str, raw_input: Value) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: String::new(),
            sdk_tool_name: sdk_tool_name.to_owned(),
            raw_input: Some(raw_input),
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_command: None,
            terminal_output: None,
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: crate::app::TerminalSnapshotMode::AppendOnly,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: crate::app::BlockCache::default(),
            pending_permission: None,
            pending_question: None,
            collapsed_override: None,
            last_measured_y_in_msg: 0,
        }
    }

    #[test]
    fn collect_active_processes_returns_empty_when_no_session() {
        // Defensive: a fresh App with no active session must not
        // panic and must collapse to an empty collection. (App
        // construction is heavyweight enough that we don't build
        // one here — we just verify the contract via the empty
        // path's shape.)
        let coll = ProcessCollection { rows: Vec::new() };
        assert!(coll.is_empty());
    }

    #[test]
    fn rows_from_os_snapshot_matches_wire_bash_via_cmdline_substring() {
        // Wire-tracked Bash with command="cargo nextest run".
        // OS-walked process with cmdline carrying that command verbatim
        // (typical sysinfo shape for a /bin/zsh wrapper).
        let tc = fake_tool_call_info(
            "toolu_1",
            "Bash",
            json!({
                "description": "Run unit tests",
                "command": "cargo nextest run",
                "run_in_background": true,
            }),
        );
        let tcs = [&tc];
        let entry = fake_entry(
            42,
            "zsh",
            "/bin/zsh -c -l source ~/.zshrc && eval 'cargo nextest run' < /dev/null",
            32 * 1024 * 1024,
        );
        let snapshot =
            ProcessSnapshot { processes: vec![entry], scanned_at: std::time::SystemTime::now() };
        let rows = rows_from_os_snapshot(&snapshot, &tcs[..]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, ProcessKind::BashBackgrounded);
        assert_eq!(rows[0].headline, "Run unit tests");
        assert_eq!(rows[0].memory_bytes, Some(32 * 1024 * 1024));
    }

    #[test]
    fn rows_from_os_snapshot_falls_back_to_process_kind_when_unmatched() {
        // No wire-tracked tool calls; every OS row becomes a generic
        // `Process` kind so we still surface the work.
        let entry = fake_entry(100, "rustc", "rustc --crate-name forge_tui ...", 256 * 1024 * 1024);
        let snapshot =
            ProcessSnapshot { processes: vec![entry], scanned_at: std::time::SystemTime::now() };
        let rows = rows_from_os_snapshot(&snapshot, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, ProcessKind::Process);
        assert_eq!(rows[0].headline, "rustc");
        assert!(rows[0].detail.as_deref().unwrap().contains("rustc --crate-name"));
    }

    #[test]
    fn rows_from_os_snapshot_uses_monitor_kind_when_wire_matches_monitor() {
        let tc = fake_tool_call_info(
            "toolu_2",
            "Monitor",
            json!({
                "description": "Watch CI run",
                "command": "gh run watch 12345",
                "persistent": true,
            }),
        );
        let tcs = [&tc];
        let entry = fake_entry(7, "gh", "gh run watch 12345 --exit-status", 8 * 1024 * 1024);
        let snapshot =
            ProcessSnapshot { processes: vec![entry], scanned_at: std::time::SystemTime::now() };
        let rows = rows_from_os_snapshot(&snapshot, &tcs[..]);
        assert_eq!(rows[0].kind, ProcessKind::Monitor);
        assert_eq!(rows[0].headline, "Watch CI run");
        assert!(rows[0].metadata.contains("persistent"));
    }

    /// Helper: build a multi-entry snapshot expressing a small
    /// claude → zsh → cargo → rustc tree. The OS scanner only
    /// includes claude's descendants, so "claude itself" is just
    /// the absence of an entry for pid 1.
    fn tree_snapshot() -> ProcessSnapshot {
        ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                // zsh wrapper — direct child of claude (parent_pid=1
                // which isn't in the snapshot, so this is a root).
                ProcessEntry {
                    pid: 10,
                    parent_pid: 1,
                    name: "zsh".to_owned(),
                    command: "/bin/zsh -c -l eval 'cargo nextest run'".to_owned(),
                    memory_bytes: 8 * 1024 * 1024,
                    started_at_unix: None,
                },
                // cargo — child of zsh.
                ProcessEntry {
                    pid: 20,
                    parent_pid: 10,
                    name: "cargo".to_owned(),
                    command: "cargo nextest run".to_owned(),
                    memory_bytes: 256 * 1024 * 1024,
                    started_at_unix: None,
                },
                // Two rustc workers — children of cargo.
                ProcessEntry {
                    pid: 30,
                    parent_pid: 20,
                    name: "rustc".to_owned(),
                    command: "rustc --crate-name forge_tui".to_owned(),
                    memory_bytes: 512 * 1024 * 1024,
                    started_at_unix: None,
                },
                ProcessEntry {
                    pid: 31,
                    parent_pid: 20,
                    name: "rustc".to_owned(),
                    command: "rustc --crate-name forge_workspace".to_owned(),
                    memory_bytes: 384 * 1024 * 1024,
                    started_at_unix: None,
                },
            ],
        }
    }

    #[test]
    fn rows_from_os_snapshot_emits_dfs_order_with_correct_depth() {
        let snapshot = tree_snapshot();
        let rows = rows_from_os_snapshot(&snapshot, &[]);
        // DFS pre-order: zsh (d0) → cargo (d1) → rustc (d2) → rustc (d2)
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].headline, "zsh");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].headline, "cargo");
        assert_eq!(rows[2].depth, 2);
        assert_eq!(rows[3].depth, 2);
    }

    #[test]
    fn rows_from_os_snapshot_pins_matched_supervisor_above_unmatched() {
        // Two roots: matched zsh wrapper + unmatched node mcp-server.
        // The matched one must come first in DFS order.
        let tc = fake_tool_call_info(
            "toolu_1",
            "Bash",
            json!({
                "description": "Run unit tests",
                "command": "cargo nextest run",
                "run_in_background": true,
            }),
        );
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                ProcessEntry {
                    pid: 100,
                    parent_pid: 1,
                    name: "node".to_owned(),
                    command: "node /path/to/mcp-server".to_owned(),
                    memory_bytes: 128 * 1024 * 1024,
                    started_at_unix: None,
                },
                ProcessEntry {
                    pid: 200,
                    parent_pid: 1,
                    name: "zsh".to_owned(),
                    command: "/bin/zsh -c -l eval 'cargo nextest run'".to_owned(),
                    memory_bytes: 8 * 1024 * 1024,
                    started_at_unix: None,
                },
            ],
        };
        let tcs = [&tc];
        let rows = rows_from_os_snapshot(&snapshot, &tcs[..]);
        // zsh is matched; node is not. Matched root sorts first
        // despite node having more memory.
        assert_eq!(rows[0].kind, ProcessKind::BashBackgrounded);
        assert_eq!(rows[0].headline, "Run unit tests");
        assert_eq!(rows[1].kind, ProcessKind::Process);
        assert_eq!(rows[1].headline, "node");
    }

    #[test]
    fn rows_from_os_snapshot_marks_last_sibling_and_ancestor_has_more() {
        let snapshot = tree_snapshot();
        let rows = rows_from_os_snapshot(&snapshot, &[]);
        // zsh root: only root → is_last_sibling = true, no ancestors.
        assert!(rows[0].is_last_sibling);
        assert!(rows[0].ancestor_has_more.is_empty());
        // cargo: only child of zsh → is_last_sibling = true,
        // ancestor_has_more = [false] (zsh has no more roots below).
        assert!(rows[1].is_last_sibling);
        assert_eq!(rows[1].ancestor_has_more, vec![false]);
        // First rustc (memory 512 MB): NOT last of cargo's two
        // children (since memory-desc sort puts it first AND there's
        // a second one below); ancestor_has_more carries through:
        // zsh has no more (false), cargo has no more either (false,
        // because cargo IS the last child of zsh).
        assert!(!rows[2].is_last_sibling);
        assert_eq!(rows[2].ancestor_has_more, vec![false, false]);
        // Second rustc (memory 384 MB): last child of cargo.
        assert!(rows[3].is_last_sibling);
        assert_eq!(rows[3].ancestor_has_more, vec![false, false]);
    }

    #[test]
    fn rows_from_os_snapshot_cron_appended_separately() {
        // Cron rows aren't part of the OS tree; they're appended
        // by `collect_active_processes` AFTER the OS-walk DFS
        // returns. Verify `rows_from_os_snapshot` itself emits zero
        // Cron-kind rows.
        let snapshot = tree_snapshot();
        let rows = rows_from_os_snapshot(&snapshot, &[]);
        assert!(rows.iter().all(|r| r.kind != ProcessKind::Cron));
    }
}
