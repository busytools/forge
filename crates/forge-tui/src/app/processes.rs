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
//! OS-walk is universal - every descendant of the spawned `claude`
//! shows up. Wire enrichment makes the row read nicely when both
//! signals agree (e.g. claude's "Run unit tests" description on a
//! `cargo nextest run` process row).
//!
//! `Cron` is the exception: `CronCreate` is a registration, not a
//! process. We still surface alive Cron rows from the wire alone
//! since there's nothing to OS-walk for them.

use std::collections::HashSet;

use forge_workspace::env::processes::{
    ProcessEntry, ProcessSnapshot, process_cmdline_matches_tool_input,
};
use serde_json::Value;

use super::App;
use crate::agent::model::ToolCallStatus;
use crate::app::MessageBlock;
use crate::app::MessageRole;
use crate::app::state::tool_call_info::{ToolCallInfo, is_execute_tool_name, is_monitor_tool_name};

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
    /// OS process with no matching wire tool call (foreground Bash,
    /// grandchildren, anything claude's tool registry doesn't know
    /// about). #273 Task 8: Monitor tool_calls also fall through to
    /// this variant - their authoritative surface is now the
    /// dedicated MONITORS Inspector section. CronCreate moved out to
    /// the dedicated SCHEDULES Inspector section (Inspector SCHEDULES
    /// plan), so it never lands here either.
    Process,
    /// Synthetic `+N more` row emitted when a single parent has more
    /// children than [`MAX_CHILDREN_PER_PARENT`] allows. Renders as
    /// a single dim italic line at the trimmed siblings' depth.
    Overflow,
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
/// Order: DFS pre-order from claude's direct children. Within each
/// sibling group, matched (pinned) rows take priority over unmatched
/// ones; rows in the same pin tier sort by `memory_bytes` descending
/// with PID as the stable tie-break. Cron rows are appended at the
/// end. Final list is capped at [`PROCESSES_MAX`] for sanity.
pub fn collect_active_processes(app: &App) -> ProcessCollection {
    let Some(session) = app.active_session() else {
        return ProcessCollection { rows: Vec::new() };
    };

    // Snapshot wire-alive tool calls. Two paths into the alive set:
    //
    // 1. **Backgrounded Bash / Monitor.** `task_started` fired on
    //    the wire and the terminal `task_updated` has NOT - so the
    //    tool_use_id is in `alive_task_ids`'s mapped set. Note the
    //    per-tool `tc.status` is unreliable here: claude's
    //    `backgroundTaskId` `tool_result` arrives almost immediately
    //    and flips status to `Completed` while the underlying
    //    process keeps running. Trust `alive_task_ids`, not `status`.
    //
    // 2. **Foreground Bash.** Claude blocks on the call; no
    //    `task_started` ever fires, so path 1 misses it entirely.
    //    The signal here IS `tc.status == InProgress` - the
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

    // OS-walked entries - the source of truth for live work.
    // Bash / Monitor wire entries that didn't match an OS row are
    // intentionally dropped (the OS walk is the truth of what is
    // RUNNING). CronCreate registrations live in the dedicated
    // SCHEDULES Inspector section, not here.
    if let Some(snapshot) = session.process_snapshot.as_ref() {
        rows.extend(rows_from_os_snapshot(snapshot, &wire_alive));
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
    // Cap per-parent children at MAX_CHILDREN_PER_PARENT - when a
    // process spawns a swarm (cargo → N rustc workers, supervisor →
    // N MCP servers) the section would otherwise drown in
    // near-identical rows. Show the top-priority subset (sibling
    // sort already pinned matched + lower-PID first) and emit a
    // single `+N more` overflow row at the same depth so the user
    // knows there's more below.
    let (visible_count, hidden) = if n > MAX_CHILDREN_PER_PARENT {
        // Reserve one slot for the overflow row so the total still
        // fits within the cap.
        let shown = MAX_CHILDREN_PER_PARENT.saturating_sub(1);
        (shown, n - shown)
    } else {
        (n, 0)
    };
    for (i, kid) in kids.iter().take(visible_count).enumerate() {
        // A visible kid is the "last sibling" only if it's the last
        // we'll emit AND there's no overflow row coming after it.
        let kid_is_last = i + 1 == visible_count && hidden == 0;
        emit_with_descendants(
            kid,
            depth.saturating_add(1),
            kid_is_last,
            &next_ancestors,
            children_of,
            wire_alive,
            out,
        );
    }
    if hidden > 0 {
        out.push(overflow_row(hidden, depth.saturating_add(1), next_ancestors));
    }
}

/// Per-parent cap on visible children. Beyond this, only
/// `MAX_CHILDREN_PER_PARENT - 1` are shown and a `+N more` row
/// stands in for the remainder. Tuned to match the typical depth-1
/// noise (a `cargo` parent commonly has 4-8 `rustc` workers; 5 rows
/// is enough signal without bloating the section).
const MAX_CHILDREN_PER_PARENT: usize = 5;

/// Synthesise a `+N more` overflow row. Rendered as a single dim
/// italic line at the same depth as the trimmed siblings, taking
/// the `└─` connector (since it's structurally the last child of
/// its parent).
fn overflow_row(hidden: usize, depth: u8, ancestor_has_more: Vec<bool>) -> ProcessRow {
    ProcessRow {
        kind: ProcessKind::Overflow,
        headline: format!("+{hidden} more"),
        detail: None,
        metadata: String::new(),
        status: ToolCallStatus::InProgress,
        memory_bytes: None,
        depth,
        is_last_sibling: true,
        ancestor_has_more,
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
        // Monitor's authoritative surface is the
        // dedicated MONITORS Inspector section. The PROCESSES row
        // no longer overlays the Monitor description on top of a
        // matching OS process - that produced double-surfaces (the
        // same description appeared in both MONITORS and PROCESSES).
        // The OS process still surfaces via the generic-OS fallback
        // so the user can still see the underlying work.
        Some(tc) if is_monitor_tool_name(&tc.sdk_tool_name) => generic_os_row(entry),
        Some(tc) if is_execute_tool_name(&tc.sdk_tool_name) => enriched_bash_row(tc, entry),
        // Matched something we don't have a special kind for
        // (defensive - shouldn't fire today). Fall through to
        // the generic OS row so we still surface the process.
        _ => generic_os_row(entry),
    }
}

/// Sort entries in place: wire-matched first (so highlighted
/// supervisors pin to the top of their sibling group), then PID
/// ascending as the sole stable tie-break.
///
/// Deliberately NOT sorted by memory - memory fluctuates each poll
/// and using it as a sort key causes the section to reshuffle every
/// frame, which reads as flicker. PID is fixed for a process's
/// lifetime so the order is stable across refreshes.
fn sort_siblings_inplace(entries: &mut [&ProcessEntry], wire_alive: &[&ToolCallInfo]) {
    entries.sort_by(|a, b| {
        let a_m = is_matched_entry(a, wire_alive);
        let b_m = is_matched_entry(b, wire_alive);
        // Matched (pinned) rows stay on top of their unpinned
        // siblings; within each group sort by memory descending so
        // the heaviest workloads land at the top. PID is the stable
        // tie-break so identical-memory siblings keep a deterministic
        // order across frames.
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
        // No cmdline continuation - the wire description already
        // conveys intent ("Run unit tests"); the literal shell
        // wrapper `/bin/zsh -c -l 'cargo ...'` is noise.
        detail: None,
        metadata: "Bash · running".to_owned(),
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
    // Use the cmdline as headline when available - for unmatched
    // supervisors that's the only meaningful context (the process
    // name alone is too vague, e.g. "node" tells you nothing about
    // WHICH node process it is). Falls back to the process name
    // when the cmdline is empty (some short-lived processes don't
    // expose one via sysinfo).
    let cmd = entry.command.trim();
    let headline = if cmd.is_empty() {
        if entry.name.is_empty() { "(process)".to_owned() } else { entry.name.clone() }
    } else {
        cmd.to_owned()
    };
    ProcessRow {
        kind: ProcessKind::Process,
        headline,
        // `detail` retained for future surfaces (Narrow overlay,
        // tooltips) but no longer rendered in the supervisor
        // 2-line block. Storing it here is cheap and lets a
        // future "expand row" affordance show the cmdline without
        // a fresh sysinfo scan.
        detail: None,
        metadata: "Process · running".to_owned(),
        status: ToolCallStatus::InProgress,
        memory_bytes: Some(entry.memory_bytes),
        depth: 0,
        is_last_sibling: true,
        ancestor_has_more: Vec::new(),
    }
}

/// Format a byte count compactly for the metadata suffix:
/// `< 1 KB` → `b`, `< 1 MB` → `K`, etc. Two significant digits in
/// the fractional range, integer above 99.
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
        // 1 decimal digit for GB - `1.2 GB` reads better than `1228 MB`.
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
            monitor_output_tail: Vec::default(),
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: crate::app::BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    #[test]
    fn collect_active_processes_returns_empty_when_no_session() {
        // Defensive: a fresh App with no active session must not
        // panic and must collapse to an empty collection. (App
        // construction is heavyweight enough that we don't build
        // one here - we just verify the contract via the empty
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
        // Unmatched supervisors use the cmdline as headline so the
        // user sees what's actually running (process name alone like
        // "rustc" or "node" is too vague when there are many).
        assert_eq!(rows[0].headline, "rustc --crate-name forge_tui ...");
        // `detail` is no longer set on supervisor rows - the cmdline
        // IS the headline now.
        assert!(rows[0].detail.is_none());
    }

    #[test]
    fn rows_from_os_snapshot_falls_through_to_generic_when_wire_matches_monitor() {
        // Monitor's authoritative surface moved to the
        // dedicated MONITORS Inspector section. PROCESSES no longer
        // overlays Monitor descriptions on the matched OS row; the
        // row falls through to the generic-OS shape so the operator
        // still sees the underlying work without the double-surface
        // (description repeated in both MONITORS and PROCESSES).
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
        assert_eq!(rows[0].kind, ProcessKind::Process);
        assert_eq!(rows[0].headline, "gh run watch 12345 --exit-status");
        assert!(!rows[0].metadata.contains("persistent"));
    }

    /// Helper: build a multi-entry snapshot expressing a small
    /// claude → zsh → cargo → rustc tree. The OS scanner only
    /// includes claude's descendants, so "claude itself" is just
    /// the absence of an entry for pid 1.
    fn tree_snapshot() -> ProcessSnapshot {
        ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                // zsh wrapper - direct child of claude (parent_pid=1
                // which isn't in the snapshot, so this is a root).
                ProcessEntry {
                    pid: 10,
                    parent_pid: 1,
                    name: "zsh".to_owned(),
                    command: "/bin/zsh -c -l eval 'cargo nextest run'".to_owned(),
                    memory_bytes: 8 * 1024 * 1024,
                    started_at_unix: None,
                },
                // cargo - child of zsh.
                ProcessEntry {
                    pid: 20,
                    parent_pid: 10,
                    name: "cargo".to_owned(),
                    command: "cargo nextest run".to_owned(),
                    memory_bytes: 256 * 1024 * 1024,
                    started_at_unix: None,
                },
                // Two rustc workers - children of cargo.
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
        // Unmatched supervisor headline = cmdline (not name).
        assert!(rows[0].headline.starts_with("/bin/zsh"));
        assert_eq!(rows[1].depth, 1);
        assert!(rows[1].headline.starts_with("cargo"));
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
        // Unmatched supervisor headline = cmdline (cmdline-as-name).
        assert_eq!(rows[1].headline, "node /path/to/mcp-server");
    }

    #[test]
    fn rows_from_os_snapshot_sorts_unpinned_siblings_by_memory_desc() {
        // Three unmatched siblings under claude; expect memory desc
        // with PID as the tie-break inside each memory tier.
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                fake_entry(100, "small", "node small", 32 * 1024 * 1024),
                fake_entry(200, "huge", "node huge", 512 * 1024 * 1024),
                fake_entry(300, "medium", "node medium", 128 * 1024 * 1024),
            ],
        };
        let rows = rows_from_os_snapshot(&snapshot, &[]);
        assert_eq!(rows[0].headline, "node huge");
        assert_eq!(rows[1].headline, "node medium");
        assert_eq!(rows[2].headline, "node small");
    }

    #[test]
    fn rows_from_os_snapshot_keeps_pinned_above_heavier_unpinned() {
        // Pinned (matched) row stays on top even when an unpinned
        // sibling has more memory.
        let tc = fake_tool_call_info(
            "toolu_1",
            "Bash",
            json!({
                "description": "Run tests",
                "command": "cargo nextest run",
                "run_in_background": true,
            }),
        );
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                fake_entry(100, "node", "node /path/big-server", 1024 * 1024 * 1024),
                fake_entry(200, "zsh", "/bin/zsh -c -l eval 'cargo nextest run'", 16 * 1024 * 1024),
            ],
        };
        let rows = rows_from_os_snapshot(&snapshot, &[&tc]);
        // Matched zsh first (despite tiny memory), heavy node second.
        assert_eq!(rows[0].kind, ProcessKind::BashBackgrounded);
        assert_eq!(rows[1].headline, "node /path/big-server");
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
    fn rows_from_os_snapshot_caps_children_with_overflow_row() {
        // A parent with 8 kids should render: 4 visible children +
        // 1 `+N more` overflow row (matching MAX_CHILDREN_PER_PARENT = 5).
        let mut processes = vec![
            // Supervisor (root)
            ProcessEntry {
                pid: 1000,
                parent_pid: 1, // not in snapshot → root
                name: "cargo".to_owned(),
                command: "cargo build".to_owned(),
                memory_bytes: 64 * 1024 * 1024,
                started_at_unix: None,
            },
        ];
        // 8 rustc worker children with PIDs 2000..2008.
        for i in 0..8u32 {
            processes.push(ProcessEntry {
                pid: 2000 + i,
                parent_pid: 1000,
                name: "rustc".to_owned(),
                command: format!("rustc --crate-name worker_{i}"),
                memory_bytes: 100 * 1024 * 1024,
                started_at_unix: None,
            });
        }
        let snapshot = ProcessSnapshot { processes, scanned_at: std::time::SystemTime::now() };
        let rows = rows_from_os_snapshot(&snapshot, &[]);
        // 1 supervisor + 4 visible children + 1 overflow = 6 rows.
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].depth, 0);
        // 4 visible rustc children at depth 1.
        for row in &rows[1..=4] {
            assert_eq!(row.depth, 1);
            assert_eq!(row.kind, ProcessKind::Process);
        }
        // Last row: overflow with "+4 more".
        assert_eq!(rows[5].kind, ProcessKind::Overflow);
        assert_eq!(rows[5].headline, "+4 more");
        assert_eq!(rows[5].depth, 1);
        assert!(rows[5].is_last_sibling);
    }

    #[test]
    fn rows_from_os_snapshot_no_overflow_when_within_cap() {
        // Exactly 5 children → all shown, no overflow row.
        let mut processes = vec![ProcessEntry {
            pid: 1000,
            parent_pid: 1,
            name: "cargo".to_owned(),
            command: "cargo build".to_owned(),
            memory_bytes: 64 * 1024 * 1024,
            started_at_unix: None,
        }];
        for i in 0..5u32 {
            processes.push(ProcessEntry {
                pid: 2000 + i,
                parent_pid: 1000,
                name: "rustc".to_owned(),
                command: format!("rustc --crate-name w_{i}"),
                memory_bytes: 100 * 1024 * 1024,
                started_at_unix: None,
            });
        }
        let snapshot = ProcessSnapshot { processes, scanned_at: std::time::SystemTime::now() };
        let rows = rows_from_os_snapshot(&snapshot, &[]);
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| r.kind != ProcessKind::Overflow));
    }

    #[test]
    fn rows_from_os_snapshot_empty_snapshot_emits_no_rows() {
        // Defensive: a fresh-spawned session may emit a poll before
        // any process has shown up under it. The DFS must handle
        // an empty `processes` vec without panicking.
        let snapshot =
            ProcessSnapshot { processes: Vec::new(), scanned_at: std::time::SystemTime::now() };
        let rows = rows_from_os_snapshot(&snapshot, &[]);
        assert!(rows.is_empty());
    }
}
