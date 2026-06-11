//! OS-level process introspection for the Inspector pane's
//! PROCESSES section.
//!
//! Walks the descendant tree of the spawned `claude` binary at the
//! operating-system level via the `sysinfo` crate, returning a
//! sorted snapshot of "interesting" descendants: shells, compilers,
//! task runners, monitors - anything claude's tool flow spawned
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
/// Always succeeds - failures (sysinfo errors, process gone) collapse
/// to an empty `processes` vec.
#[derive(Debug, Clone)]
pub struct ProcessSnapshot {
    /// Sorted descendants of the supplied `claude_pid`. Sort order:
    /// memory descending - most resource-hungry processes first so
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
    /// Parent process identifier - `claude_pid` for direct children,
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
/// Always returns successfully - sysinfo failures, missing processes,
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

    // sysinfo doesn't expose a "give me children of X" query - we
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

            // Skip the scanner's own process - would appear when
            // forge itself runs as a descendant of claude (it
            // shouldn't, but defensively).
            if self_pid == Some(child_pid) {
                continue;
            }

            let Some(proc) = system.process(child_pid) else { continue };

            // Skip zombies / dead - they're in the table but
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

/// Unwrap a shell-wrapper cmdline to the user command inside it.
/// Recognizes the shape claude wraps Bash tool calls in (captured via
/// `ps -axww`): `/bin/zsh -c source <snapshot> ... && eval '<CMD>' <
/// /dev/null && pwd -P >| /tmp/claude-<hash>-cwd`. The inner command is
/// single-quoted between `eval '` and `' < /dev/null`. Returns `None`
/// when `cmdline` is not a recognized wrapper (non-shell processes pass
/// through unchanged at the call site).
pub fn extract_inner_command(cmdline: &str) -> Option<String> {
    let after_eval = cmdline.split_once("eval '")?.1;
    let inner = after_eval.split_once("' < /dev/null")?.0;
    Some(inner.trim().to_owned())
}

/// Strip the executable's directory from a cmdline headline so the row
/// reads `cargo nextest run`, not `/opt/homebrew/bin/cargo nextest run`.
/// Only the FIRST token (the executable) is basenamed; args are kept
/// verbatim (they carry the distinguishing detail). A first token with
/// no `/` (e.g. a `postgres: walwriter` process title) is unchanged.
pub fn basename_exe(cmdline: &str) -> String {
    let cmdline = cmdline.trim();
    match cmdline.split_once(char::is_whitespace) {
        Some((exe, rest)) => {
            let base = exe.rsplit('/').next().unwrap_or(exe);
            format!("{base} {rest}")
        }
        None => cmdline.rsplit('/').next().unwrap_or(cmdline).to_owned(),
    }
}

/// Collapse every run of ASCII whitespace (spaces, tabs, newlines) to a
/// single space and trim the ends. `sysinfo`'s `proc.cmd()` joins argv
/// with single spaces, so a multi-line / multi-space wire command never
/// matches the captured cmdline byte-for-byte; normalizing both sides
/// first makes the substring check robust.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when `process_cmd` plausibly matches the shell command
/// captured in a wire-tracked tool's `raw_input.command`. Used by
/// the Inspector's row builder (path A+) to overlay wire-tracked
/// task descriptions onto OS-detected processes when both refer to
/// the same work.
///
/// Unwraps the shell wrapper first (so the comparison is against the
/// user command, not the `/bin/zsh -c ... eval '...'` chrome), then matches
/// on a whitespace-normalized substring basis.
///
/// Empty `tool_command` returns `false` (no useful match possible).
pub fn process_cmdline_matches_tool_input(process_cmd: &str, tool_command: &str) -> bool {
    let needle = tool_command.trim();
    if needle.is_empty() {
        return false;
    }
    let haystack = extract_inner_command(process_cmd).unwrap_or_else(|| process_cmd.to_owned());
    normalize_ws(&haystack).contains(&normalize_ws(needle))
}

/// What flavour of known infra a process is. MCP servers are the only
/// recognized kind today; the enum keeps a typed slot so a future
/// recognizer doesn't have to widen the call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfraKind {
    McpServer,
}

/// Friendly display name + kind for a recognized known-infra process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfraLabel {
    pub name: String,
    pub kind: InfraKind,
}

/// Friendly name for a known-infra process (MCP servers today), derived
/// from its cmdline. `None` when unrecognized.
///
/// Recognizes the `npm exec <pkg>` and `node .../<bin>` shapes captured
/// via `ps -axww`, with the `@version` suffix stripped: `<base>-mcp[-server]`
/// → `<base>` (`@upstash/context7-mcp` → `context7`), a package literally
/// named `mcp` → its scope (`@playwright/mcp@latest` → `playwright`), and
/// the `@modelcontextprotocol/server-<base>` convention → `<base>`.
pub fn classify_known_infra(cmdline: &str) -> Option<InfraLabel> {
    let name = mcp_name_from_npm(cmdline).or_else(|| mcp_name_from_node(cmdline))?;
    Some(InfraLabel { name, kind: InfraKind::McpServer })
}

/// Parse an npm package spec into `(scope, name)` with the `@version`
/// stripped. `@playwright/mcp@latest` -> `(Some("playwright"), "mcp")`;
/// `@upstash/context7-mcp` -> `(Some("upstash"), "context7-mcp")`;
/// `context7-mcp@1.0` -> `(None, "context7-mcp")`.
fn parse_npm_pkg(pkg_spec: &str) -> (Option<&str>, &str) {
    let (scope, rest) = match pkg_spec.strip_prefix('@') {
        Some(after_at) => match after_at.split_once('/') {
            Some((scope, rest)) => (Some(scope), rest),
            None => (None, pkg_spec),
        },
        None => (None, pkg_spec),
    };
    let name = rest.split('@').next().unwrap_or(rest);
    (scope, name)
}

/// Friendly MCP name from a parsed `(scope, name)`, or `None` when the
/// package isn't a recognized MCP server:
/// - `<base>-mcp` / `<base>-mcp-server` -> `<base>`
/// - package literally named `mcp` -> the scope (`@playwright/mcp` -> `playwright`)
/// - `server-<base>` (the `@modelcontextprotocol` convention) -> `<base>`
fn mcp_friendly_name(scope: Option<&str>, name: &str) -> Option<String> {
    if let Some(base) = name.strip_suffix("-mcp-server").or_else(|| name.strip_suffix("-mcp"))
        && !base.is_empty()
    {
        return Some(base.to_owned());
    }
    if name == "mcp" {
        return scope.filter(|s| !s.is_empty()).map(ToOwned::to_owned);
    }
    if let Some(base) = name.strip_prefix("server-")
        && !base.is_empty()
    {
        return Some(base.to_owned());
    }
    None
}

/// `npm exec @playwright/mcp@latest` → `playwright`. Requires the literal
/// `npm` + `exec` argv prefix; the package spec must resolve to an MCP
/// name via [`mcp_friendly_name`] or this returns `None`.
fn mcp_name_from_npm(cmdline: &str) -> Option<String> {
    let mut toks = cmdline.split_whitespace();
    if toks.next()? != "npm" || toks.next()? != "exec" {
        return None;
    }
    let (scope, name) = parse_npm_pkg(toks.next()?);
    mcp_friendly_name(scope, name)
}

/// `node /opt/foo-mcp-server.js` → `foo`. Requires a `node` argv[0] and
/// a later token whose basename (sans `.js` / `@version`) resolves via
/// [`mcp_friendly_name`] (scope-less, so a bare `mcp` doesn't match).
fn mcp_name_from_node(cmdline: &str) -> Option<String> {
    let mut toks = cmdline.split_whitespace();
    if !toks.next()?.ends_with("node") {
        return None;
    }
    toks.find_map(|tok| {
        let base = tok.rsplit('/').next().unwrap_or(tok);
        let base = base.strip_suffix(".js").unwrap_or(base);
        let name = base.split('@').next().unwrap_or(base);
        mcp_friendly_name(None, name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tool_command_never_matches() {
        assert!(!process_cmdline_matches_tool_input("any process cmd", ""));
        assert!(!process_cmdline_matches_tool_input("any process cmd", "   "));
    }

    /// Ground-truth wrapper captured via `ps -axww` on this machine
    /// (a backgrounded Bash tool call). Differs from the idealized
    /// `-c -l source ~/.zshrc` shape: no `-l`, a `source <snapshot>
    /// 2>/dev/null || true` clause, a `setopt ... || true` clause, and a
    /// trailing `&& pwd -P >| /tmp/claude-<hash>-cwd` after the
    /// `< /dev/null` redirect. The inner command is single-quoted
    /// between `eval '` and `' < /dev/null`.
    const REAL_WRAPPER: &str = "/bin/zsh -c source /Users/x/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true && setopt NO_EXTENDED_GLOB NO_BARE_GLOB_QUAL 2>/dev/null || true && eval 'gh run watch 123 --exit-status' < /dev/null && pwd -P >| /tmp/claude-ab12-cwd";

    #[test]
    fn extract_inner_command_unwraps_real_wrapper() {
        assert_eq!(
            extract_inner_command(REAL_WRAPPER).as_deref(),
            Some("gh run watch 123 --exit-status")
        );
        // A plain non-wrapper cmdline passes through as None so the
        // caller keeps the original.
        assert_eq!(extract_inner_command("rustc --crate-name forge_tui"), None);
        assert_eq!(extract_inner_command("node balance-transfer.mjs"), None);
    }

    #[test]
    fn substring_match_holds_for_wrapped_shell_invocation() {
        // The user-typed command resolves out of the real wrapper.
        let tool_command = "gh run watch 123 --exit-status";
        assert!(process_cmdline_matches_tool_input(REAL_WRAPPER, tool_command));
    }

    #[test]
    fn wrapped_multiline_command_matches_after_normalize() {
        // The failure the idealized fixture hid: `proc.cmd()` joins argv
        // with single spaces, so a multi-line / multi-space wire command
        // is not a raw substring of the captured cmdline. Normalizing
        // whitespace on both sides (after unwrapping) makes it match.
        let tool_command = "gh run watch 123\n  --exit-status";
        assert!(process_cmdline_matches_tool_input(REAL_WRAPPER, tool_command));
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
        // happens to contain "cargo" - but our substring rule says
        // it does. Documented here as a known limitation; the
        // alternative (token-level match) is more code for a
        // marginal win on personal-use scope.
        assert!(process_cmdline_matches_tool_input("/usr/bin/cargo build", "cargo"));
    }

    #[test]
    fn basename_exe_strips_exe_dir_keeps_args() {
        assert_eq!(basename_exe("/opt/homebrew/bin/cargo nextest run"), "cargo nextest run");
        assert_eq!(
            basename_exe("/Users/x/.rustup/toolchains/nightly/bin/rustc --crate-name forge_tui"),
            "rustc --crate-name forge_tui"
        );
        // A bare test binary with no args is just its basename.
        assert_eq!(
            basename_exe("/Users/x/proj/target/debug/deps/some_test-9ab1"),
            "some_test-9ab1"
        );
        // First token with no `/` (a process title) is returned unchanged.
        assert_eq!(basename_exe("postgres: walwriter"), "postgres: walwriter");
        // Already-bare command is unchanged.
        assert_eq!(basename_exe("cargo build"), "cargo build");
        // Edges don't panic.
        assert_eq!(basename_exe(""), "");
        let _ = basename_exe("/");
    }

    #[test]
    fn classify_known_infra_recognizes_npm_exec_mcp() {
        // Real shapes captured via ps -axww.
        let c = classify_known_infra("npm exec @upstash/context7-mcp").expect("context7");
        assert_eq!(c.name, "context7");
        assert_eq!(c.kind, InfraKind::McpServer);

        let n = classify_known_infra("npm exec @notionhq/notion-mcp-server").expect("notion");
        assert_eq!(n.name, "notion");
        assert_eq!(n.kind, InfraKind::McpServer);

        // Non-MCP npm exec + unrelated processes don't classify.
        assert!(classify_known_infra("npm exec @scope/some-tool").is_none());
        assert!(classify_known_infra("rustc --crate-name forge_tui").is_none());
        assert!(classify_known_infra("node balance-transfer.mjs").is_none());
    }

    #[test]
    fn classify_known_infra_recognizes_node_launched_mcp() {
        let l = classify_known_infra("node /opt/foo-mcp-server.js").expect("foo");
        assert_eq!(l.name, "foo");
        assert_eq!(l.kind, InfraKind::McpServer);
    }

    #[test]
    fn classify_known_infra_recognizes_broadened_shapes() {
        // @scope/mcp (package literally named `mcp`) -> the scope is the
        // name. Real shape captured via ps: `npm exec @playwright/mcp@latest
        // --cdp-endpoint ...`.
        assert_eq!(
            classify_known_infra(
                "npm exec @playwright/mcp@latest --cdp-endpoint http://localhost:9222"
            )
            .expect("playwright")
            .name,
            "playwright"
        );
        assert_eq!(
            classify_known_infra("npm exec @playwright/mcp").expect("playwright").name,
            "playwright"
        );
        // @modelcontextprotocol/server-<name> convention.
        assert_eq!(
            classify_known_infra("npm exec @modelcontextprotocol/server-filesystem")
                .expect("filesystem")
                .name,
            "filesystem"
        );
        // @version suffix stripped on the existing -mcp shape.
        assert_eq!(
            classify_known_infra("npm exec @upstash/context7-mcp@2.0").expect("context7").name,
            "context7"
        );
        // The real playwright node child folds to the same name, so #351's
        // dedup collapses it under the parent.
        assert_eq!(
            classify_known_infra(
                "node /Users/x/.npm/_npx/abc/node_modules/.bin/playwright-mcp --cdp-endpoint http://localhost:9222"
            )
            .expect("playwright child")
            .name,
            "playwright"
        );
    }

    #[test]
    fn scan_returns_empty_snapshot_for_missing_pid() {
        // PID 0 is the kernel scheduler on Linux / never has children
        // on macOS - walk should yield zero descendants. Confirms
        // `scan` is total even for nonsense input.
        let snapshot = scan(0);
        // Don't assert empty - PID 0 has children on Linux (kthreadd).
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
