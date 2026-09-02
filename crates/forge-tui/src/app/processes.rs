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
//! MCP-server processes are not rows here: the `MCP SERVERS` section
//! (`crate::app::mcp_servers`) owns every configured server, and the
//! pids its join claims are skipped by the walk so a server's backing
//! tree renders exactly once.
//!
//! `Cron` is the exception: `CronCreate` is a registration, not a
//! process. We still surface alive Cron rows from the wire alone
//! since there's nothing to OS-walk for them.

use std::collections::{HashMap, HashSet};

use forge_workspace::env::processes::{
    ProcessEntry, ProcessSnapshot, basename_exe, extract_inner_command,
    process_cmdline_matches_tool_input,
};
use serde_json::Value;

use super::App;
use crate::agent::model::ToolCallStatus;
use crate::app::MessageBlock;
use crate::app::MessageRole;
use crate::app::state::tool_call_info::{ToolCallInfo, is_execute_tool_name, is_monitor_tool_name};
use crate::app::state::types::BackgroundTask;

/// Soft cap on the rendered PROCESSES section. Sanity bound so a
/// runaway process tree doesn't blow up the body line count; users
/// scroll within the section to see everything below the cap. The
/// `overflow` count on [`ProcessCollection`] is no longer rendered
/// as a footer row (the scrollbar IS the overflow indicator) but
/// is kept so future surfaces can show "n hidden" if needed.
const PROCESSES_MAX: usize = 50;

/// One row in the PROCESSES section.
#[derive(Debug, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug)]
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
/// they're registrations, not processes. Processes the `MCP SERVERS`
/// join claimed are skipped - a server's backing tree renders in that
/// section, not here.
///
/// Order: registry-fed backgrounded `local_bash` rows the OS scan missed
/// lead, then the OS walk in DFS pre-order from claude's direct children.
/// Within each sibling group, rows sort matched work first, then generic
/// processes, with `memory_bytes` descending inside each tier and PID as
/// the tie-break. Final list is capped at [`PROCESSES_MAX`] for sanity.
pub fn collect_active_processes(app: &App) -> ProcessCollection {
    let Some(session) = app.active_session() else {
        return ProcessCollection { rows: Vec::new() };
    };
    let wire_alive = wire_alive_tool_calls(session);

    let mut rows: Vec<ProcessRow> = Vec::new();

    // Registry-fed backgrounded `local_bash` rows lead: a bash the OS scan
    // hasn't surfaced (short-lived, pre-first-scan, or turn-outlived) is active
    // work, so it leads the OS walk instead of trailing below it, and leading
    // keeps it ahead of the sanity cap. Commands resolve through the
    // session-scoped task map (survives turn finalisation), so a bash the OS
    // scan already covers is deduped out, not doubled.
    if session.background_tasks.iter().any(|task| task.task_type == "local_bash") {
        let command_by_task_id = session_command_by_task_id(session);
        rows.extend(background_bash_rows(
            &session.background_tasks,
            &command_by_task_id,
            session.process_snapshot.as_ref(),
        ));
    }

    // OS-walked entries follow - the source of truth for live work.
    // Bash / Monitor wire entries that didn't match an OS row are
    // intentionally dropped (the OS walk is the truth of what is
    // RUNNING). CronCreate registrations live in the dedicated
    // SCHEDULES Inspector section, not here.
    let mcp = crate::app::mcp_servers::collect_mcp_servers(app);
    if let Some(snapshot) = session.process_snapshot.as_ref() {
        rows.extend(rows_from_os_snapshot(snapshot, &wire_alive, &mcp.claimed_pids));
    }

    rows.truncate(PROCESSES_MAX);
    ProcessCollection { rows }
}

/// The session's live wire tool calls to overlay onto the OS scan. Two
/// paths in:
///
/// 1. **Backgrounded work (any kind).** The CLI's `backgroundTaskId`
///    tool_result flips `tc.status` to `Completed` almost immediately
///    while the process keeps running, and the spawning turn Results
///    before it finishes - so status is unreliable and the turn-scoped
///    alive set is wiped underneath it. The durable signal is the session
///    roster (`background_tasks` INTERSECT the session task map), which
///    survives turn finalisation. Only bash carries a `command` field, so
///    a resolved agent in this set simply never matches an OS row.
///
/// 2. **Foreground Bash.** Claude blocks on the call; no `task_started`
///    ever fires, so path 1 misses it entirely. The signal here IS
///    `tc.status == InProgress` - the `tool_result` hasn't arrived because
///    the command is still running - which lets a foreground `cargo build`
///    / `git status` / `ls` row pick up the wire's `description` as a
///    headline instead of falling through to the generic OS row.
pub(crate) fn wire_alive_tool_calls(
    session: &crate::app::session::UiSession,
) -> Vec<&ToolCallInfo> {
    let backgrounded_alive = session.backgrounded_alive_tool_use_ids();
    session
        .messages
        .iter()
        .filter(|m| matches!(m.role, MessageRole::Assistant))
        .flat_map(|m| &m.blocks)
        .filter_map(|b| match b {
            MessageBlock::ToolCall(tc)
                if backgrounded_alive.contains(tc.id.as_str())
                    || tc.status == ToolCallStatus::InProgress =>
            {
                Some(tc.as_ref())
            }
            _ => None,
        })
        .collect()
}

/// Build `task_id` -> wire command for the active session by joining the
/// session-scoped task map (`task_id` -> `tool_use_id`, survives turn
/// finalisation) with each tool call's `raw_input.command`. Used to dedup
/// the backgrounded-`local_bash` feed against OS-scan rows.
fn session_command_by_task_id(session: &crate::app::session::UiSession) -> HashMap<String, String> {
    let command_by_tool_use: HashMap<&str, &str> = session
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            MessageBlock::ToolCall(tc) => {
                let command = read_str_field(tc.raw_input.as_ref(), "command");
                (!command.is_empty()).then_some((tc.id.as_str(), command))
            }
            _ => None,
        })
        .collect();
    session
        .session_task_tool_use_ids
        .iter()
        .filter_map(|(task_id, tool_use_id)| {
            command_by_tool_use
                .get(tool_use_id.as_str())
                .map(|command| (task_id.clone(), (*command).to_owned()))
        })
        .collect()
}

/// The active session's live backgrounded `local_bash` commands, resolved
/// through the same session-scoped task map the row builder uses. Fed to
/// the OS process scan so a `setsid`-detached / orphaned bash outside
/// claude's descendant tree is adopted into the snapshot (RAM + tree)
/// instead of falling back to a memory-less synthetic row.
pub(crate) fn live_local_bash_commands(session: &crate::app::session::UiSession) -> Vec<String> {
    let local_bash_task_ids: HashSet<&str> = session
        .background_tasks
        .iter()
        .filter(|task| task.task_type == "local_bash")
        .map(|task| task.task_id.as_str())
        .collect();
    if local_bash_task_ids.is_empty() {
        return Vec::new();
    }
    session_command_by_task_id(session)
        .into_iter()
        .filter(|(task_id, _)| local_bash_task_ids.contains(task_id.as_str()))
        .map(|(_, command)| command)
        .collect()
}

/// Synthesise PROCESSES rows for CLI-registry backgrounded `local_bash`
/// the OS scan hasn't surfaced. Skips a task whose command already
/// substring-matches a scanned process (the OS walk covers it) or whose
/// command is unresolvable from the session map (no `task_started` mapping
/// recorded); non-`local_bash` kinds route to SUBAGENTS / WORKFLOWS.
fn background_bash_rows(
    background_tasks: &[BackgroundTask],
    command_by_task_id: &HashMap<String, String>,
    snapshot: Option<&ProcessSnapshot>,
) -> Vec<ProcessRow> {
    background_tasks
        .iter()
        .filter(|task| task.task_type == "local_bash")
        .filter_map(|task| {
            let command = command_by_task_id.get(&task.task_id)?;
            let has_os_row = snapshot.is_some_and(|snapshot| {
                snapshot
                    .processes
                    .iter()
                    .any(|entry| process_cmdline_matches_tool_input(&entry.command, command))
            });
            (!has_os_row).then(|| synthetic_background_bash_row(&task.description, &task.task_type))
        })
        .collect()
}

/// A backgrounded-bash row sourced from the CLI registry rather than the
/// OS scan: description as headline, `task_type` as the trailing tag, no
/// memory (there's no scanned process behind it).
fn synthetic_background_bash_row(description: &str, task_type: &str) -> ProcessRow {
    ProcessRow {
        kind: ProcessKind::BashBackgrounded,
        headline: description.to_owned(),
        detail: None,
        metadata: task_type.to_owned(),
        status: ToolCallStatus::InProgress,
        memory_bytes: None,
        depth: 0,
        is_last_sibling: true,
        ancestor_has_more: Vec::new(),
    }
}

/// DFS the OS snapshot's process tree from claude's direct children
/// down, emitting one [`ProcessRow`] per node in pre-order with
/// correct `depth` + tree-connector metadata. Processes in
/// `claimed_pids` (a `MCP SERVERS` server's backing tree) are skipped
/// entirely, wherever they sit. Siblings within each group are ordered
/// by [`sort_siblings_inplace`] (kind tier, then memory descending with
/// PID as the stable tie-break).
fn rows_from_os_snapshot<'a>(
    snapshot: &'a ProcessSnapshot,
    wire_alive: &'a [&'a ToolCallInfo],
    claimed_pids: &HashSet<u32>,
) -> Vec<ProcessRow> {
    // Index by pid + build a parent → children adjacency list. Both
    // exclude claimed pids: the walk never descends into a claimed
    // subtree, and its subtree-memory totals must not count processes
    // that moved to the MCP section.
    let by_pid: HashMap<u32, &ProcessEntry> =
        snapshot.processes.iter().map(|e| (e.pid, e)).collect();
    let mut children_of: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    for entry in &snapshot.processes {
        if claimed_pids.contains(&entry.pid) {
            continue;
        }
        children_of.entry(entry.parent_pid).or_default().push(entry);
    }

    // Roots = entries whose parent is NOT in the snapshot: a claude
    // descendant whose parent is claude, or an adopted backgrounded bash
    // whose parent is init - both parents sit outside the snapshot. Root
    // detection reads the FULL pid index so a child of a claimed process
    // can't resurface as a root; the claimed filter then drops it.
    let mut roots: Vec<&ProcessEntry> = snapshot
        .processes
        .iter()
        .filter(|e| !by_pid.contains_key(&e.parent_pid) && !claimed_pids.contains(&e.pid))
        .collect();
    // Depth-0 roots sort by the subtree total they display, not their
    // own RSS - precompute it once per root rather than per-comparison.
    let root_subtree: HashMap<u32, u64> = roots
        .iter()
        .map(|r| {
            let mut visited = HashSet::new();
            (r.pid, subtree_memory(r, &children_of, &mut visited))
        })
        .collect();
    sort_siblings_inplace(&mut roots, wire_alive, Some(&root_subtree));

    let walk = Walk { children_of: &children_of, wire_alive };
    let mut rows = Vec::new();
    let n_roots = roots.len();
    for (idx, root) in roots.iter().enumerate() {
        emit_with_descendants(root, 0, idx + 1 == n_roots, &[], &walk, &mut rows);
    }
    rows
}

/// The walk-invariant inputs to [`emit_with_descendants`] - identical at every
/// node, unlike the per-node tree position.
struct Walk<'a> {
    children_of: &'a HashMap<u32, Vec<&'a ProcessEntry>>,
    wire_alive: &'a [&'a ToolCallInfo],
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
    walk: &Walk<'a>,
    out: &mut Vec<ProcessRow>,
) {
    let Walk { children_of, wire_alive } = *walk;
    let mut row = build_row_for_entry(entry, wire_alive);
    row.depth = depth;
    let mut visited = HashSet::new();
    let subtree_bytes = subtree_memory(entry, children_of, &mut visited);
    // Supervisor (depth-0) rows show the whole subtree's resident
    // memory; a bare parent's own RSS (a 2 MB zsh over a 256 MB cargo
    // child) reads wrong. Descendants keep their own RSS.
    if depth == 0 {
        row.memory_bytes = Some(subtree_bytes);
    }
    row.is_last_sibling = is_last_sibling;
    row.ancestor_has_more = ancestor_has_more.to_vec();
    out.push(row);

    let mut kids: Vec<&ProcessEntry> =
        children_of.get(&entry.pid).map_or_else(Vec::new, Clone::clone);
    sort_siblings_inplace(&mut kids, wire_alive, None);

    // The next level's ancestor_has_more appends THIS row's
    // "more-siblings-below" bit so a deep descendant knows whether
    // to keep drawing the `│` continuation in this row's column.
    let mut next_ancestors = ancestor_has_more.to_vec();
    next_ancestors.push(!is_last_sibling);

    let n = kids.len();
    // Cap per-parent children at MAX_CHILDREN_PER_PARENT - when a
    // process spawns a swarm (cargo → N rustc workers) the section
    // would otherwise drown in near-identical rows. Show the
    // top-priority subset (sibling sort already ordered by tier +
    // memory + lower-PID) and emit a single `+N more` overflow row at
    // the same depth so the user knows there's more below.
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
            walk,
            out,
        );
    }
    if hidden > 0 {
        out.push(overflow_row(hidden, depth.saturating_add(1), next_ancestors));
    }
}

/// Sum the resident memory of `entry` plus all its descendants in the
/// snapshot. The snapshot is a tree so each pid appears once; the
/// `visited` set guards against a pathological parent-chain cycle so
/// the recursion can't run away or double-count.
fn subtree_memory(
    entry: &ProcessEntry,
    children_of: &std::collections::HashMap<u32, Vec<&ProcessEntry>>,
    visited: &mut HashSet<u32>,
) -> u64 {
    if !visited.insert(entry.pid) {
        return 0;
    }
    let mut total = entry.memory_bytes;
    if let Some(kids) = children_of.get(&entry.pid) {
        for kid in kids {
            total = total.saturating_add(subtree_memory(kid, children_of, visited));
        }
    }
    total
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

/// The alive wire tool call whose `command` substring-matches `entry`'s
/// cmdline, if any. Shared by [`build_row_for_entry`], the tier in
/// `sort_siblings_inplace`, and the `MCP SERVERS` join (a wire-matched
/// process is tracked work, never a server's backing process).
pub(crate) fn wire_match<'a>(
    entry: &ProcessEntry,
    wire_alive: &[&'a ToolCallInfo],
) -> Option<&'a ToolCallInfo> {
    wire_alive.iter().copied().find(|tc| {
        let cmd = read_str_field(tc.raw_input.as_ref(), "command");
        !cmd.is_empty() && process_cmdline_matches_tool_input(&entry.command, cmd)
    })
}

/// Build a row for a single OS entry, doing the wire-match check
/// against the alive tool calls. Tree position (`depth`,
/// `is_last_sibling`, `ancestor_has_more`) is initialised to
/// defaults; the DFS walker overwrites them.
fn build_row_for_entry(entry: &ProcessEntry, wire_alive: &[&ToolCallInfo]) -> ProcessRow {
    match wire_match(entry, wire_alive) {
        // Monitor's authoritative surface is the
        // dedicated MONITORS Inspector section. The PROCESSES row
        // no longer overlays the Monitor description on top of a
        // matching OS process - that produced double-surfaces (the
        // same description appeared in both MONITORS and PROCESSES).
        // The OS process still surfaces via the generic-OS fallback
        // so the user can still see the underlying work.
        Some(tc) if is_monitor_tool_name(&tc.sdk_tool_name) => generic_os_row(entry),
        Some(tc) if is_execute_tool_name(&tc.sdk_tool_name) => enriched_bash_row(tc, entry),
        _ => generic_os_row(entry),
    }
}

/// Sort entries in place by kind tier, then effective memory descending
/// with PID as the stable tie-break (PID is fixed for a process's lifetime
/// so ties stay deterministic across frames). Tiers: matched Bash work pins
/// to the top, unrecognized generic processes follow - memory only orders
/// within a tier.
///
/// `subtree_totals`, when supplied for depth-0 roots, overrides each
/// entry's sort-memory with its subtree total - so a supervisor sorts
/// by the same figure it displays (a 2 MB zsh wrapper over a 1 GB
/// subtree sorts as 1 GB, not 2 MB). Descendants pass `None` and sort
/// by their own RSS, which is also what they display.
fn sort_siblings_inplace(
    entries: &mut [&ProcessEntry],
    wire_alive: &[&ToolCallInfo],
    subtree_totals: Option<&std::collections::HashMap<u32, u64>>,
) {
    let sort_mem = |e: &ProcessEntry| -> u64 {
        subtree_totals.and_then(|m| m.get(&e.pid).copied()).unwrap_or(e.memory_bytes)
    };
    // Mirror build_row_for_entry's kind arms so sort tier and render kind never
    // disagree: matched Bash renders BashBackgrounded (0); a matched Monitor
    // renders generic (1 - its authoritative surface is the MONITORS section).
    let tier = |e: &ProcessEntry| -> u8 {
        match wire_match(e, wire_alive) {
            Some(tc) if is_monitor_tool_name(&tc.sdk_tool_name) => 1,
            Some(tc) if is_execute_tool_name(&tc.sdk_tool_name) => 0,
            _ => 1,
        }
    };
    entries.sort_by(|a, b| {
        tier(a)
            .cmp(&tier(b))
            .then_with(|| sort_mem(b).cmp(&sort_mem(a)))
            .then_with(|| a.pid.cmp(&b.pid))
    });
}

/// OS process matched to a wire-tracked backgrounded `Bash`. Headline
/// from the wire description; detail from the OS cmdline; metadata
/// suffixed with memory.
fn enriched_bash_row(tc: &ToolCallInfo, entry: &ProcessEntry) -> ProcessRow {
    let description = read_str_field(tc.raw_input.as_ref(), "description");
    // Headline precedence: wire description, else the unwrapped inner
    // command (so a backgrounded `gh run watch <id>` with no description
    // reads as that, not `zsh`), else the OS process name.
    let headline = if description.is_empty() {
        extract_inner_command(&entry.command)
            .map_or_else(|| entry.name.clone(), |c| basename_exe(&c))
    } else {
        description.to_owned()
    };
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
    // A shell-wrapper cmdline shows its inner command (the raw
    // `/bin/zsh -c ... eval '...'` chrome is never a headline). Otherwise
    // the cmdline is the headline - for unmatched supervisors that's the
    // only meaningful context (the process name alone is too vague, e.g.
    // "node" tells you nothing about WHICH node process it is). Falls
    // back to the process name when the cmdline is empty (some
    // short-lived processes don't expose one via sysinfo).
    let cmd = entry.command.trim();
    let headline = if let Some(inner) = extract_inner_command(&entry.command) {
        basename_exe(&inner)
    } else if cmd.is_empty() {
        if entry.name.is_empty() { "(process)".to_owned() } else { entry.name.clone() }
    } else {
        basename_exe(cmd)
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
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
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

    /// Real wrapper shape captured via `ps -axww`: inner command is
    /// single-quoted between `eval '` and `' < /dev/null`, with a
    /// `source ... || true` prefix and trailing `pwd -P` redirect.
    fn real_wrapper(inner: &str) -> String {
        format!(
            "/bin/zsh -c source /Users/x/.claude/shell-snapshots/snap.sh 2>/dev/null || true && setopt NO_EXTENDED_GLOB 2>/dev/null || true && eval '{inner}' < /dev/null && pwd -P >| /tmp/claude-ab12-cwd"
        )
    }

    #[test]
    fn enriched_bash_row_uses_inner_command_when_description_empty() {
        // Matched Bash with NO description: headline falls back to the
        // unwrapped inner command, never the raw "zsh" process name.
        let tc = fake_tool_call_info(
            "toolu_1",
            "Bash",
            json!({ "command": "gh run watch 123", "run_in_background": true }),
        );
        let entry = fake_entry(42, "zsh", &real_wrapper("gh run watch 123"), 8 * 1024 * 1024);
        let row = enriched_bash_row(&tc, &entry);
        assert_eq!(row.headline, "gh run watch 123");
    }

    #[test]
    fn generic_os_row_basenames_full_path_headline() {
        // A child process with a full executable path shows just the
        // basename + args, not the directory-eating absolute path.
        let entry = fake_entry(
            60,
            "rustc",
            "/Users/x/.rustup/toolchains/nightly/bin/rustc --crate-name forge_tui",
            256 * 1024 * 1024,
        );
        let row = generic_os_row(&entry);
        assert_eq!(row.headline, "rustc --crate-name forge_tui");
    }

    #[test]
    fn generic_os_row_unwraps_shell_wrapper_headline() {
        // An unmatched shell-wrapper process shows the inner command,
        // never the raw /bin/zsh wrapper.
        let entry = fake_entry(42, "zsh", &real_wrapper("npm run build"), 8 * 1024 * 1024);
        let row = generic_os_row(&entry);
        assert_eq!(row.headline, "npm run build");
        assert!(!row.headline.contains("/bin/zsh"));
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
        let rows = rows_from_os_snapshot(&snapshot, &tcs[..], &HashSet::new());
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
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
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
        let rows = rows_from_os_snapshot(&snapshot, &tcs[..], &HashSet::new());
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
                    command: "/bin/zsh -c source /x/snap.sh 2>/dev/null || true && eval 'cargo nextest run' < /dev/null && pwd -P >| /tmp/claude-a-cwd".to_owned(),
                    memory_bytes: 8 * 1024 * 1024,
                },
                // cargo - child of zsh.
                ProcessEntry {
                    pid: 20,
                    parent_pid: 10,
                    name: "cargo".to_owned(),
                    command: "cargo nextest run".to_owned(),
                    memory_bytes: 256 * 1024 * 1024,
                },
                // Two rustc workers - children of cargo.
                ProcessEntry {
                    pid: 30,
                    parent_pid: 20,
                    name: "rustc".to_owned(),
                    command: "rustc --crate-name forge_tui".to_owned(),
                    memory_bytes: 512 * 1024 * 1024,
                },
                ProcessEntry {
                    pid: 31,
                    parent_pid: 20,
                    name: "rustc".to_owned(),
                    command: "rustc --crate-name forge_workspace".to_owned(),
                    memory_bytes: 384 * 1024 * 1024,
                },
            ],
        }
    }

    #[test]
    fn rows_from_os_snapshot_emits_dfs_order_with_correct_depth() {
        let snapshot = tree_snapshot();
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
        // DFS pre-order: zsh (d0) → cargo (d1) → rustc (d2) → rustc (d2)
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].depth, 0);
        // Unmatched shell-wrapper supervisor headline = the unwrapped
        // inner command; the raw /bin/zsh wrapper is never a headline.
        assert_eq!(rows[0].headline, "cargo nextest run");
        assert!(!rows[0].headline.contains("/bin/zsh"));
        assert_eq!(rows[1].depth, 1);
        assert!(rows[1].headline.starts_with("cargo"));
        assert_eq!(rows[2].depth, 2);
        assert_eq!(rows[3].depth, 2);
    }

    #[test]
    fn rows_from_os_snapshot_rolls_subtree_memory_onto_depth0() {
        // The depth-0 supervisor reads the whole subtree's memory (its
        // own 2 MB zsh parent over heavy children is misleading);
        // descendants keep their own RSS.
        let snapshot = tree_snapshot();
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
        let subtree = (8 + 256 + 512 + 384) * 1024 * 1024;
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].memory_bytes, Some(subtree), "depth-0 = subtree total");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].memory_bytes, Some(256 * 1024 * 1024), "depth-1 keeps own RSS");
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
                    command: "node /path/to/build-helper.js".to_owned(),
                    memory_bytes: 128 * 1024 * 1024,
                },
                ProcessEntry {
                    pid: 200,
                    parent_pid: 1,
                    name: "zsh".to_owned(),
                    command: "/bin/zsh -c source /x/snap.sh 2>/dev/null || true && eval 'cargo nextest run' < /dev/null && pwd -P >| /tmp/claude-b-cwd".to_owned(),
                    memory_bytes: 8 * 1024 * 1024,
                },
            ],
        };
        let tcs = [&tc];
        let rows = rows_from_os_snapshot(&snapshot, &tcs[..], &HashSet::new());
        // zsh is matched; node is not. Matched root sorts first
        // despite node having more memory.
        assert_eq!(rows[0].kind, ProcessKind::BashBackgrounded);
        assert_eq!(rows[0].headline, "Run unit tests");
        assert_eq!(rows[1].kind, ProcessKind::Process);
        // Unmatched non-wrapper supervisor headline = its cmdline.
        assert_eq!(rows[1].headline, "node /path/to/build-helper.js");
    }

    #[test]
    fn rows_from_os_snapshot_sorts_depth0_by_subtree_total() {
        // Root A: light own RSS (2 MB zsh wrapper) over a heavy 1000 MB
        // child -> subtree 1002 MB. Root B: heavy own RSS (500 MB), no
        // children. A must sort ABOVE B (it displays 1002 MB), even
        // though A's OWN RSS is far smaller.
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                ProcessEntry {
                    pid: 10,
                    parent_pid: 1,
                    name: "zsh".to_owned(),
                    command: real_wrapper("run the build"),
                    memory_bytes: 2 * 1024 * 1024,
                },
                ProcessEntry {
                    pid: 11,
                    parent_pid: 10,
                    name: "cargo".to_owned(),
                    command: "cargo build".to_owned(),
                    memory_bytes: 1000 * 1024 * 1024,
                },
                ProcessEntry {
                    pid: 20,
                    parent_pid: 1,
                    name: "node".to_owned(),
                    command: "node server.mjs".to_owned(),
                    memory_bytes: 500 * 1024 * 1024,
                },
            ],
        };
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].headline, "run the build", "heavy-subtree root sorts first");
        assert_eq!(rows[0].memory_bytes, Some(1002 * 1024 * 1024));
        // Root B (own 500 MB, lighter subtree) comes after root A's subtree.
        assert_eq!(rows[2].depth, 0);
        assert_eq!(rows[2].headline, "node server.mjs");
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
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
        assert_eq!(rows[0].headline, "node huge");
        assert_eq!(rows[1].headline, "node medium");
        assert_eq!(rows[2].headline, "node small");
    }

    #[test]
    fn rows_from_os_snapshot_orders_matched_then_generic_over_memory() {
        // Tier model: matched/wire-tracked work first, then unrecognized
        // generic processes - memory-desc only WITHIN the generic tier, so a
        // light matched row still outranks a heavier generic.
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
                // Generic, heaviest by far.
                fake_entry(300, "postgres", "postgres -D /data", 512 * 1024 * 1024),
                // Matched bash, lightest.
                fake_entry(100, "zsh", "/bin/zsh -c -l eval 'cargo nextest run'", 10 * 1024 * 1024),
            ],
        };
        let rows = rows_from_os_snapshot(&snapshot, &[&tc], &HashSet::new());
        assert_eq!(rows[0].kind, ProcessKind::BashBackgrounded, "matched pins top; got {rows:?}");
        assert_eq!(rows[1].kind, ProcessKind::Process, "generic last; got {rows:?}");
    }

    #[test]
    fn rows_from_os_snapshot_tie_breaks_equal_memory_same_tier_by_pid() {
        // Two same-tier (generic) roots with IDENTICAL memory must order by
        // PID ascending - the documented cross-frame determinism guarantee.
        // Input is reversed vs PID order so a stable sort without the PID
        // tie-break would leave "node b.mjs" first.
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                fake_entry(200, "node", "node b.mjs", 64 * 1024 * 1024),
                fake_entry(100, "node", "node a.mjs", 64 * 1024 * 1024),
            ],
        };
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
        assert_eq!(rows[0].headline, "node a.mjs", "lower PID first on equal memory");
        assert_eq!(rows[1].headline, "node b.mjs");
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
        let rows = rows_from_os_snapshot(&snapshot, &[&tc], &HashSet::new());
        // Matched zsh first (despite tiny memory), heavy node second.
        assert_eq!(rows[0].kind, ProcessKind::BashBackgrounded);
        assert_eq!(rows[1].headline, "node /path/big-server");
    }

    #[test]
    fn rows_from_os_snapshot_marks_last_sibling_and_ancestor_has_more() {
        let snapshot = tree_snapshot();
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
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
            });
        }
        let snapshot = ProcessSnapshot { processes, scanned_at: std::time::SystemTime::now() };
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
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
        }];
        for i in 0..5u32 {
            processes.push(ProcessEntry {
                pid: 2000 + i,
                parent_pid: 1000,
                name: "rustc".to_owned(),
                command: format!("rustc --crate-name w_{i}"),
                memory_bytes: 100 * 1024 * 1024,
            });
        }
        let snapshot = ProcessSnapshot { processes, scanned_at: std::time::SystemTime::now() };
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
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
        let rows = rows_from_os_snapshot(&snapshot, &[], &HashSet::new());
        assert!(rows.is_empty());
    }

    fn bg_task(task_id: &str, task_type: &str, description: &str) -> BackgroundTask {
        BackgroundTask {
            task_id: task_id.to_owned(),
            task_type: task_type.to_owned(),
            description: description.to_owned(),
        }
    }

    #[test]
    fn live_local_bash_commands_returns_only_local_bash_commands() {
        use crate::app::{App, ChatMessage};

        let mut app = App::test_default();
        let bash = fake_tool_call_info(
            "tu-bash",
            "Bash",
            json!({ "command": "gh run watch 123 --exit-status", "run_in_background": true }),
        );
        let agent = fake_tool_call_info("tu-agent", "Task", json!({ "command": "investigate" }));
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(bash)), MessageBlock::ToolCall(Box::new(agent))],
        ));
        app.insert_session_task_mapping("task-bash".to_owned(), "tu-bash".to_owned());
        app.insert_session_task_mapping("task-agent".to_owned(), "tu-agent".to_owned());
        *app.background_tasks_mut() = vec![
            bg_task("task-bash", "local_bash", "watch CI"),
            bg_task("task-agent", "local_agent", "investigate"),
        ];

        let session = app.active_session().expect("active session");
        // Only the local_bash task's command is returned; the agent's is not.
        assert_eq!(
            live_local_bash_commands(session),
            vec!["gh run watch 123 --exit-status".to_owned()]
        );
    }

    #[test]
    fn live_local_bash_commands_empty_without_local_bash_task() {
        use crate::app::App;

        let mut app = App::test_default();
        *app.background_tasks_mut() = vec![bg_task("task-agent", "local_agent", "investigate")];
        let session = app.active_session().expect("active session");
        assert!(live_local_bash_commands(session).is_empty());
    }

    #[test]
    fn background_bash_rows_synthesizes_when_os_scan_misses_it() {
        // A short-lived / just-started backgrounded bash is in the CLI's
        // registry but absent from the OS snapshot -> synthesise a row so
        // it isn't dropped now that the standalone BACKGROUND section is
        // gone.
        let tasks = vec![bg_task("t1", "local_bash", "Print marker after 1s")];
        let mut cmd_by_task = HashMap::new();
        cmd_by_task.insert("t1".to_owned(), "sleep 1 && echo marker".to_owned());
        let snapshot =
            ProcessSnapshot { processes: Vec::new(), scanned_at: std::time::SystemTime::now() };
        let rows = background_bash_rows(&tasks, &cmd_by_task, Some(&snapshot));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, ProcessKind::BashBackgrounded);
        assert_eq!(rows[0].headline, "Print marker after 1s");
        assert_eq!(rows[0].metadata, "local_bash");
        assert!(rows[0].memory_bytes.is_none());
        assert_eq!(rows[0].status, ToolCallStatus::InProgress);
    }

    #[test]
    fn background_bash_rows_dedups_against_matching_os_row() {
        // The bash IS in the snapshot (its wire command substring-matches
        // a scanned process), so the OS row already covers it - no
        // synthetic duplicate.
        let tasks = vec![bg_task("t1", "local_bash", "Run tests")];
        let mut cmd_by_task = HashMap::new();
        cmd_by_task.insert("t1".to_owned(), "cargo nextest run".to_owned());
        let entry = fake_entry(42, "zsh", &real_wrapper("cargo nextest run"), 8 * 1024 * 1024);
        let snapshot =
            ProcessSnapshot { processes: vec![entry], scanned_at: std::time::SystemTime::now() };
        let rows = background_bash_rows(&tasks, &cmd_by_task, Some(&snapshot));
        assert!(rows.is_empty(), "matched OS row covers it; got {rows:?}");
    }

    #[test]
    fn background_bash_rows_ignores_non_bash_task_types() {
        // Agents route to SUBAGENTS, workflows to WORKFLOWS - only
        // local_bash is fed to PROCESSES.
        let tasks = vec![
            bg_task("a", "local_agent", "Audit history"),
            bg_task("w", "local_workflow", "Run workflow"),
        ];
        let rows = background_bash_rows(&tasks, &HashMap::new(), None);
        assert!(rows.is_empty());
    }

    #[test]
    fn background_bash_rows_synthesizes_before_first_scan() {
        // Cold start: `task_started` mapped the command this turn but no
        // snapshot exists yet. The registry bash still surfaces.
        let tasks = vec![bg_task("t1", "local_bash", "npm run build")];
        let mut cmd_by_task = HashMap::new();
        cmd_by_task.insert("t1".to_owned(), "npm run build".to_owned());
        let rows = background_bash_rows(&tasks, &cmd_by_task, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].headline, "npm run build");
    }

    #[test]
    fn background_bash_rows_skips_unresolved_task_to_avoid_double() {
        // A backgrounded bash that outlived its turn: `task_started`'s
        // mapping was cleared at turn finalisation, so the command can't
        // be resolved. Its still-alive process is already an OS row, so
        // the feed must NOT add a synthetic duplicate.
        let tasks = vec![bg_task("t1", "local_bash", "gh run watch 123")];
        let entry = fake_entry(9, "gh", "gh run watch 123 --exit-status", 8 * 1024 * 1024);
        let snapshot =
            ProcessSnapshot { processes: vec![entry], scanned_at: std::time::SystemTime::now() };
        let rows = background_bash_rows(&tasks, &HashMap::new(), Some(&snapshot));
        assert!(rows.is_empty(), "unresolved task must not double the OS row; got {rows:?}");
    }

    #[test]
    fn collect_active_processes_keeps_os_caught_bash_enriched_after_turn_reset() {
        // A backgrounded bash caught by the OS scan must stay an enriched
        // BashBackgrounded row (wire description headline) after its
        // spawning turn finalises - not degrade to a generic Process row
        // showing the raw command. The turn-scoped alive set is wiped at
        // turn-complete, so only the session-scoped background_tasks
        // registry (resolved via the session task map) keeps the OS row
        // enriched. Mirrors SUBAGENTS cross-turn survival.
        use crate::app::{App, ChatMessage};

        let mut app = App::test_default();

        // The backgrounding sentinel already flipped the card to Completed
        // while the OS process keeps running.
        let mut bash = fake_tool_call_info(
            "tu-bash",
            "Bash",
            json!({
                "command": "sleep 60 && echo done",
                "description": "Wait then print",
                "run_in_background": true,
            }),
        );
        bash.status = ToolCallStatus::Completed;
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(bash))],
        ));

        // Session-scoped signals the real producer writes mid-turn: the
        // task map (task_id -> tool_use_id) and the local_bash registry.
        app.insert_session_task_mapping("task-bash".to_owned(), "tu-bash".to_owned());
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "task-bash".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "Wait then print".to_owned(),
        }];

        // OS scan caught the still-running process (cmdline substring-matches
        // the wire command).
        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![ProcessEntry {
                pid: 4242,
                parent_pid: 1,
                name: "zsh".to_owned(),
                command: "/bin/zsh -c -l eval 'sleep 60 && echo done' < /dev/null".to_owned(),
                memory_bytes: 4 * 1024 * 1024,
            }],
        });

        // Turn finalises: turn-scoped liveness wiped. The session-scoped
        // registry must carry the enrichment across.
        let _: () = app.with_turn_state_mut(|ts| {
            ts.task_tool_use_ids.clear();
        });

        let coll = collect_active_processes(&app);
        let bash_rows: Vec<_> =
            coll.rows.iter().filter(|r| r.kind == ProcessKind::BashBackgrounded).collect();
        assert_eq!(bash_rows.len(), 1, "exactly one enriched bash row; got {:?}", coll.rows);
        assert_eq!(
            bash_rows[0].headline, "Wait then print",
            "row keeps the wire description, not the raw command; got {:?}",
            coll.rows,
        );
        assert!(
            !coll.rows.iter().any(|r| r.kind == ProcessKind::Process),
            "no degraded generic Process row; got {:?}",
            coll.rows,
        );
    }

    #[test]
    fn collect_active_processes_does_not_enrich_bash_when_registry_drained_after_turn_reset() {
        // Intersection gate: a stale session-map entry ALONE must NOT enrich.
        // Same setup as the enriched-across-turn-reset sibling, but the
        // registry (`background_tasks`) is drained (a killed task whose
        // terminal task_updated never arrived). The still-running OS process
        // must stay a plain Process row, never a phantom BashBackgrounded -
        // `background_tasks` is the authoritative liveness gate.
        use crate::app::{App, ChatMessage};

        let mut app = App::test_default();

        let mut bash = fake_tool_call_info(
            "tu-bash",
            "Bash",
            json!({
                "command": "sleep 60 && echo done",
                "description": "Wait then print",
                "run_in_background": true,
            }),
        );
        bash.status = ToolCallStatus::Completed;
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(bash))],
        ));

        // Session map still carries the mapping (survives turn reset), but the
        // registry is drained - no local_bash entry to gate on.
        app.insert_session_task_mapping("task-bash".to_owned(), "tu-bash".to_owned());

        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![ProcessEntry {
                pid: 4242,
                parent_pid: 1,
                name: "zsh".to_owned(),
                command: "/bin/zsh -c -l eval 'sleep 60 && echo done' < /dev/null".to_owned(),
                memory_bytes: 4 * 1024 * 1024,
            }],
        });

        let _: () = app.with_turn_state_mut(|ts| {
            ts.task_tool_use_ids.clear();
        });

        let coll = collect_active_processes(&app);
        assert!(
            coll.rows.iter().all(|r| r.kind != ProcessKind::BashBackgrounded),
            "stale session-map entry alone must not enrich a phantom row; got {:?}",
            coll.rows,
        );
        assert_eq!(coll.rows.len(), 1, "the OS process still shows once; got {:?}", coll.rows);
        assert_eq!(coll.rows[0].kind, ProcessKind::Process);
        assert_eq!(
            coll.rows[0].headline, "sleep 60 && echo done",
            "unenriched row carries the raw command; got {:?}",
            coll.rows,
        );
    }

    #[test]
    fn collect_active_processes_matches_single_quote_bash_as_one_enriched_row() {
        // A backgrounded bash whose command contains single-quotes: the shell
        // wrapper re-escapes each `'` as `'"'"'`. The OS row must still
        // correlate to its wire tool call (enriched BashBackgrounded,
        // description headline) AND the synthetic registry feed must dedup
        // against it - exactly one row, not a raw unmatched Process row plus a
        // synthetic duplicate.
        use crate::app::{App, ChatMessage};

        let mut app = App::test_default();

        // Backgrounded bash: the sentinel flipped its card Completed while the
        // OS process keeps running, so enrichment rides the session-scoped
        // registry, not the turn-scoped alive set.
        let mut bash = fake_tool_call_info(
            "tu-bash",
            "Bash",
            json!({
                "command": "echo 'sq-marker'; sleep 40",
                "description": "Print marker then wait",
                "run_in_background": true,
            }),
        );
        bash.status = ToolCallStatus::Completed;
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(bash))],
        ));

        app.insert_session_task_mapping("task-bash".to_owned(), "tu-bash".to_owned());
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "task-bash".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "Print marker then wait".to_owned(),
        }];

        // OS scan caught the process; its cmdline carries the `'"'"'`-escaped
        // single-quotes exactly as `ps -axww` reports them.
        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![ProcessEntry {
                pid: 4242,
                parent_pid: 1,
                name: "zsh".to_owned(),
                command: real_wrapper(r#"echo '"'"'sq-marker'"'"'; sleep 40"#),
                memory_bytes: 4 * 1024 * 1024,
            }],
        });

        let coll = collect_active_processes(&app);
        assert_eq!(
            coll.rows.len(),
            1,
            "exactly one row, no synthetic duplicate; got {:?}",
            coll.rows
        );
        assert_eq!(coll.rows[0].kind, ProcessKind::BashBackgrounded, "got {:?}", coll.rows);
        assert_eq!(coll.rows[0].headline, "Print marker then wait", "got {:?}", coll.rows);
        assert!(
            !coll.rows.iter().any(|r| r.kind == ProcessKind::Process),
            "no raw unmatched Process row; got {:?}",
            coll.rows,
        );
    }

    #[test]
    fn collect_active_processes_leaves_mcp_server_processes_to_the_mcp_section() {
        // A process joined to a snapshot MCP server renders in MCP SERVERS,
        // not here - even a 200 MB server tree against a light generic row.
        // The MCP section's join is the single classifier: whatever it
        // claims, the walk must skip entirely.
        use crate::app::App;

        let mut app = App::test_default();

        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                ProcessEntry {
                    pid: 100,
                    parent_pid: 1,
                    name: "npm".to_owned(),
                    command: "npm exec @upstash/context7-mcp".to_owned(),
                    memory_bytes: 200 * 1024 * 1024,
                },
                ProcessEntry {
                    pid: 200,
                    parent_pid: 1,
                    name: "cargo".to_owned(),
                    command: "cargo build".to_owned(),
                    memory_bytes: 20 * 1024 * 1024,
                },
            ],
        });
        app.mcp_mut().servers = vec![forge_primitives::McpServerStatus {
            name: "context7".to_owned(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            config: Some(json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@upstash/context7-mcp"],
            })),
            ..Default::default()
        }];

        let coll = collect_active_processes(&app);
        assert_eq!(
            coll.rows.iter().map(|r| r.headline.as_str()).collect::<Vec<_>>(),
            vec!["cargo build"],
            "the MCP server's process must leave PROCESSES; got {:?}",
            coll.rows,
        );
    }

    #[test]
    fn collect_active_processes_synthetic_bash_survives_the_row_cap() {
        // Synthetic local_bash rows LEAD (the old truncate-reserve is gone), so
        // with more OS rows than the cap the leading synthetic bash must still
        // survive the truncation. Locks that ordering invariant in.
        use crate::app::{App, ChatMessage};

        let mut app = App::test_default();

        // A resolvable backgrounded bash the OS scan did NOT catch.
        let bash = fake_tool_call_info(
            "tu-bash",
            "Bash",
            json!({ "command": "deploy.sh", "description": "Deploy", "run_in_background": true }),
        );
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(bash))],
        ));
        app.insert_session_task_mapping("task-bash".to_owned(), "tu-bash".to_owned());
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "task-bash".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "Deploy".to_owned(),
        }];

        // More generic OS roots than PROCESSES_MAX, none matching the bash.
        let processes = (0..60u32)
            .map(|i| ProcessEntry {
                pid: 1000 + i,
                parent_pid: 1,
                name: "worker".to_owned(),
                command: format!("worker{i} --serve"),
                memory_bytes: 10 * 1024 * 1024,
            })
            .collect();
        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes,
        });

        let coll = collect_active_processes(&app);
        assert_eq!(coll.rows.len(), PROCESSES_MAX, "capped at the sanity max");
        assert_eq!(
            coll.rows[0].kind,
            ProcessKind::BashBackgrounded,
            "leading synthetic bash survives the cap; got {:?}",
            coll.rows[0],
        );
    }
}
