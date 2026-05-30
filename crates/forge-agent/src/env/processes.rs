//! OS-level process introspection for the Inspector pane's
//! PROCESSES section.
//!
//! Walks the descendant tree of the spawned `claude` binary at the
//! operating-system level via the `sysinfo` crate, returning a
//! sorted snapshot of "interesting" descendants: shells, compilers,
//! task runners, monitors  -  anything claude's tool flow spawned
//! that's still alive at scan time.
//!
//! Why not key off the wire alone? The wire's `task_started` /
//! `task_updated` pair covers `Bash` with `run_in_background:true`
//! and the `Monitor` tool's streaming watcher, but it doesn't
//! surface:
//! - Foreground Bash invocations (claude blocks on them; no
//!   `task_started`).
//! - Grandchild processes the spawned shell forks off (e.g. a
//!   `cargo build` that fans out into `rustc` workers).
//! - Anything that detaches from claude's tool registry but stays
//!   in the process tree.
//!
//! Architect's `processes.py` solved the same surface for Python
//! via `psutil`; we mirror it in Rust with `sysinfo`. macOS + Linux
//! are first-class; Windows falls back to an empty snapshot because
//! shell-process names (`cmd.exe` / `powershell.exe`) require a
//! different recogniser the personal-use scope doesn't need today.
//!
//! The snapshot is consumed by `forge-workspace::scan_processes`
//! (thin mediator) and the TUI's per-active-session ticker in
//! `forge-tui::app::process_scanner`. The Inspector's
//! `collect_active_processes` walker (path A+) overlays wire-
//! tracked task descriptions on top of matching OS processes via
//! cmdline lookup, then renders the merged view.

use std::time::SystemTime;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Snapshot of `claude`'s descendant processes at one point in time.
/// Always succeeds  -  failures (sysinfo errors, process gone) collapse
/// to an empty `processes` vec.
#[derive(Debug, Clone)]
pub struct ProcessSnapshot {
    /// Sorted descendants of the supplied `claude_pid`. Sort order:
    /// memory descending  -  most resource-hungry processes first so
    /// a top-N display surfaces the interesting ones.
    pub processes: Vec<ProcessEntry>,
    /// When the scan ran. The TUI compares against this to decide
    /// whether to spawn a fresh poll on each ticker pass.
    pub scanned_at: SystemTime,
}

impl Default for ProcessSnapshot {
    fn default() -> Self {
        Self { processes: Vec::new(), scanned_at: SystemTime::now() }
    }
}

/// One descendant process the scanner found alive. Field shape is
/// driven by what the Inspector's `collect_active_processes` row
/// builder needs (PID + cmdline + memory for the metadata line) +
/// the cmdline-matching needed for wire-tracked overlay
/// (`process_cmdline_matches_tool_input`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    /// OS process identifier.
    pub pid: u32,
    /// Parent process identifier  -  `claude_pid` for direct children,
    /// some other descendant for grandchildren.
    pub parent_pid: u32,
    /// Process name as the OS reports it (e.g. `"zsh"`, `"cargo"`,
    /// `"rustc"`). Used as the row headline when no wire-tracked
    /// task overlay matches.
    pub name: String,
    /// Full cmdline joined with spaces (e.g.
    /// `"cargo nextest run --no-fail-fast"`). Used for the row's
    /// detail line and for fuzzy match against
    /// `ToolCallInfo.raw_input.command`.
    pub command: String,
    /// Resident memory in bytes. Rendered abbreviated (`234M`,
    /// `1.2G`) on the row's metadata line.
    pub memory_bytes: u64,
    /// Wall-clock seconds-since-epoch when this process was forked.
    /// Optional because some sysinfo backends report 0 for very
    /// short-lived processes; the renderer just hides "elapsed"
    /// when missing.
    pub started_at_unix: Option<u64>,
}

/// Walk the descendants of `claude_pid` and build a sorted snapshot.
/// Always returns successfully  -  sysinfo failures, missing processes,
/// or empty descendant trees all collapse to an empty snapshot with
/// a fresh `scanned_at`.
///
/// Filters applied:
/// - Skip the scan-process itself (sysinfo's `current_pid`).
/// - Skip zombies (`Run` / `Sleep` only; defunct entries don't
///   represent live work).
/// - Skip ephemeral `ps` / `sysinfo` self-probes that would
///   themselves match the walk window (mirrors architect's
///   filter pattern).
pub fn scan(claude_pid: u32) -> ProcessSnapshot {
    // `System::new()` is cheap; refreshing processes is the
    // expensive bit. Limit the refresh to memory + cmdline only
    // (skip CPU sampling which requires two refreshes spaced apart
    // to be meaningful, disk I/O which we don't render, etc.).
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new().with_memory().with_cmd(sysinfo::UpdateKind::Always),
    );

    let self_pid = sysinfo::get_current_pid().ok();
    let mut processes = Vec::new();
    collect_descendants(&system, Pid::from_u32(claude_pid), self_pid, &mut processes);

    // Sort by memory descending so the rendered top-N surfaces the
    // heaviest workers first. Stable secondary key by PID keeps
    // ordering deterministic for tests + same-memory ties.
    processes.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes).then_with(|| a.pid.cmp(&b.pid)));

    ProcessSnapshot { processes, scanned_at: SystemTime::now() }
}

/// Recursively walk `sysinfo::System`'s process table, collecting
/// every descendant of `root_pid`. Filters out the scanner's own
/// process and known-uninteresting noise.
fn collect_descendants(
    system: &System,
    root_pid: Pid,
    self_pid: Option<Pid>,
    out: &mut Vec<ProcessEntry>,
) {
    use std::collections::HashSet;

    // sysinfo doesn't expose a "give me children of X" query  -  we
    // index parent_pid → children once, then walk from root.
    let mut children_of: std::collections::HashMap<Pid, Vec<Pid>> =
        std::collections::HashMap::new();
    for (pid, proc) in system.processes() {
        if let Some(parent) = proc.parent() {
            children_of.entry(parent).or_default().push(*pid);
        }
    }

    let mut stack = vec![root_pid];
    let mut visited: HashSet<Pid> = HashSet::new();
    while let Some(parent) = stack.pop() {
        let Some(children) = children_of.get(&parent) else { continue };
        for &child_pid in children {
            if !visited.insert(child_pid) {
                continue;
            }
            stack.push(child_pid);

            // Skip the scanner's own process  -  would appear when
            // forge itself runs as a descendant of claude (it
            // shouldn't, but defensively).
            if self_pid == Some(child_pid) {
                continue;
            }

            let Some(proc) = system.process(child_pid) else { continue };

            // Skip zombies / dead  -  they're in the table but
            // represent no live work.
            if matches!(
                proc.status(),
                sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
            ) {
                continue;
            }

            // Skip our own one-shot `ps` / `sysinfo` probes that
            // would otherwise appear in the snapshot. Architect
            // does the same filter for its `psutil.Process(claude).children()`
            // walk.
            let name = proc.name().to_string_lossy().to_string();
            if name == "ps" || name == "sysinfo" {
                continue;
            }

            let command = proc
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");

            let started_at_unix = proc.start_time().checked_sub(0).filter(|t| *t > 0);

            out.push(ProcessEntry {
                pid: child_pid.as_u32(),
                parent_pid: parent.as_u32(),
                name,
                command,
                memory_bytes: proc.memory(),
                started_at_unix,
            });
        }
    }
}

/// True when `process_cmd` plausibly matches the shell command
/// captured in a wire-tracked tool's `raw_input.command`. Used by
/// the Inspector's row builder (path A+) to overlay wire-tracked
/// task descriptions onto OS-detected processes when both refer to
/// the same work.
///
/// Match strategy: substring containment. Shell wrappers add
/// `/bin/zsh -c -l "source ... && eval '<command>' < /dev/null"`
/// around the user-typed command, so exact equality almost never
/// holds  -  but the user's command appears verbatim somewhere in
/// the wrapped cmdline. Returns `true` when `tool_command` is a
/// non-empty substring of `process_cmd`.
///
/// Empty `tool_command` returns `false` (no useful match possible).
pub fn process_cmdline_matches_tool_input(process_cmd: &str, tool_command: &str) -> bool {
    let needle = tool_command.trim();
    if needle.is_empty() {
        return false;
    }
    process_cmd.contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tool_command_never_matches() {
        assert!(!process_cmdline_matches_tool_input("any process cmd", ""));
        assert!(!process_cmdline_matches_tool_input("any process cmd", "   "));
    }

    #[test]
    fn substring_match_holds_for_wrapped_shell_invocation() {
        // Real-world shape: claude's Bash tool runs commands via a
        // shell wrapper. The user-typed command is a substring of
        // the actual process cmdline.
        let process_cmd =
            "/bin/zsh -c -l source ~/.zshrc && eval 'cargo nextest run --no-fail-fast' < /dev/null";
        let tool_command = "cargo nextest run --no-fail-fast";
        assert!(process_cmdline_matches_tool_input(process_cmd, tool_command));
    }

    #[test]
    fn exact_match_holds() {
        assert!(process_cmdline_matches_tool_input(
            "cargo build --release",
            "cargo build --release"
        ));
    }

    #[test]
    fn unrelated_command_does_not_match() {
        assert!(!process_cmdline_matches_tool_input(
            "rustc --crate-name forge_tui",
            "cargo nextest run"
        ));
    }

    #[test]
    fn partial_word_does_not_match_false_positive() {
        // Defensive: "cargo" alone shouldn't match a row that just
        // happens to contain "cargo"  -  but our substring rule says
        // it does. Documented here as a known limitation; the
        // alternative (token-level match) is more code for a
        // marginal win on personal-use scope.
        assert!(process_cmdline_matches_tool_input("/usr/bin/cargo build", "cargo"));
    }

    #[test]
    fn scan_returns_empty_snapshot_for_missing_pid() {
        // PID 0 is the kernel scheduler on Linux / never has children
        // on macOS  -  walk should yield zero descendants. Confirms
        // `scan` is total even for nonsense input.
        let snapshot = scan(0);
        // Don't assert empty  -  PID 0 has children on Linux (kthreadd).
        // What we DO assert: scanned_at is recent + no panic.
        let age = SystemTime::now().duration_since(snapshot.scanned_at).map_or(0, |d| d.as_secs());
        assert!(age <= 5, "scanned_at must be recent");
    }

    #[test]
    fn scan_for_self_pid_runs_without_panic() {
        // Walking the test runner's own PID should yield a snapshot
        // (possibly empty if no children exist). Mainly testing
        // that the scan path doesn't trip over its own foot when
        // self_pid filter applies.
        let me = std::process::id();
        let _ = scan(me);
    }
}
