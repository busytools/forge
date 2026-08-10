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

use forge_primitives::McpServerStatus;
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
}

/// Walk `claude_pid`'s descendant tree plus any command-matched
/// backgrounded `local_bash` and build a sorted snapshot. Always
/// returns successfully - sysinfo failures, missing processes, or empty
/// trees all collapse to an empty snapshot with a fresh `scanned_at`.
///
/// `extra_commands` are the session's live backgrounded `local_bash`
/// commands: such a bash is `setsid`-detached (or orphaned to init when
/// its session's claude exits) so it sits outside claude's tree, and
/// adopting the command-matched wrapper plus its subtree gives it a real
/// RAM + process-tree row instead of a memory-less synthetic one.
///
/// Filters applied to every candidate: skip the scanner's own pid,
/// zombies/dead (in the table but no live work), and the one-shot `ps` /
/// `sysinfo` self-probes that would themselves match the walk window.
pub fn scan(claude_pid: u32, extra_commands: &[String]) -> ProcessSnapshot {
    // `System::new()` is cheap; refreshing processes is the expensive
    // bit. Limit the refresh to memory + cmdline only (skip CPU sampling
    // which needs two spaced refreshes, disk I/O we don't render, etc.).
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new().with_memory().with_cmd(sysinfo::UpdateKind::Always),
    );

    let self_pid = sysinfo::get_current_pid().ok();
    let candidates = live_candidates(&system, self_pid);
    let mut processes = select_snapshot(candidates, claude_pid, extra_commands);

    // Sort by memory descending so the rendered top-N surfaces the
    // heaviest workers first. Stable secondary key by PID keeps
    // ordering deterministic for tests + same-memory ties.
    processes.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes).then_with(|| a.pid.cmp(&b.pid)));

    ProcessSnapshot { processes, scanned_at: SystemTime::now() }
}

/// Build a `ProcessEntry` for every live, interesting process in the
/// table, dropping the scanner's own pid, zombies/dead, and the one-shot
/// `ps` / `sysinfo` self-probes (mirrors architect's filter pattern).
fn live_candidates(system: &System, self_pid: Option<Pid>) -> Vec<ProcessEntry> {
    let mut out = Vec::new();
    for (pid, proc) in system.processes() {
        if self_pid == Some(*pid) {
            continue;
        }
        if matches!(proc.status(), sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead) {
            continue;
        }
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
        out.push(ProcessEntry {
            pid: pid.as_u32(),
            parent_pid: proc.parent().map_or(0, Pid::as_u32),
            name,
            command,
            memory_bytes: proc.memory(),
        });
    }
    out
}

/// Select the processes that belong in the snapshot: the descendants of
/// `claude_pid`, plus any candidate whose cmdline matches one of
/// `extra_commands` together with ITS descendants (a backgrounded
/// `local_bash` living outside claude's tree). Pure over `candidates` so
/// the adoption logic is unit-testable without a live process table.
fn select_snapshot(
    candidates: Vec<ProcessEntry>,
    claude_pid: u32,
    extra_commands: &[String],
) -> Vec<ProcessEntry> {
    // sysinfo has no "children of X" query - index parent -> children
    // once, then walk.
    let mut children_of: std::collections::HashMap<u32, Vec<u32>> =
        std::collections::HashMap::new();
    for entry in &candidates {
        children_of.entry(entry.parent_pid).or_default().push(entry.pid);
    }

    let mut included: std::collections::HashSet<u32> = std::collections::HashSet::new();
    // claude's descendants (claude itself is never a row).
    include_subtree(&children_of, claude_pid, &mut included);

    // Adopt command-matched processes outside claude's tree + their
    // subtrees. Collect the roots under the immutable borrow first, then
    // walk each - the root itself joins the snapshot, unlike claude.
    if !extra_commands.is_empty() {
        let matched_roots: Vec<u32> = candidates
            .iter()
            .filter(|entry| !included.contains(&entry.pid))
            // Only a genuine claude bash wrapper is an adoptable backgrounded
            // local_bash; gating on a successful unwrap keeps a foreign process
            // that merely substring-matches the wire command out of the snapshot.
            .filter(|entry| extract_inner_command(&entry.command).is_some())
            .filter(|entry| {
                extra_commands
                    .iter()
                    .any(|cmd| process_cmdline_matches_tool_input(&entry.command, cmd))
            })
            .map(|entry| entry.pid)
            .collect();
        for root in matched_roots {
            if included.insert(root) {
                include_subtree(&children_of, root, &mut included);
            }
        }
    }

    candidates.into_iter().filter(|entry| included.contains(&entry.pid)).collect()
}

/// Insert every descendant of `root` (not `root` itself) into
/// `included`, walking the `parent -> children` index.
fn include_subtree(
    children_of: &std::collections::HashMap<u32, Vec<u32>>,
    root: u32,
    included: &mut std::collections::HashSet<u32>,
) {
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        let Some(children) = children_of.get(&pid) else { continue };
        for &child in children {
            if included.insert(child) {
                stack.push(child);
            }
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
    // Terminate at the OUTERMOST `' < /dev/null` (the eval close-quote) so a
    // command that itself contains that redirect doesn't cut off early.
    let inner = after_eval.rsplit_once("' < /dev/null")?.0;
    // The wrapper re-escapes each `'` as the POSIX sequence `'"'"'`; reverse it
    // so a single-quote command still matches its wire `raw_input.command`.
    Some(inner.trim().replace(r#"'"'"'"#, "'"))
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

/// Friendly display name + kind for a recognized known-infra process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfraLabel {
    pub name: String,
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
    Some(InfraLabel { name })
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

/// A configured MCP server distilled to what the process matcher needs:
/// the configured `name` plus the stdio launch `command` + `args` used to
/// match it against a live process cmdline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMcpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

/// Extract the stdio-launched configured MCP servers from an mcp-status
/// snapshot. A server contributes `(name, command, args)` only when its
/// `config` blob carries a string `command` (the stdio shape); url-only
/// (http/sse) servers have no local process, so they're skipped.
pub fn configured_mcp_servers(servers: &[McpServerStatus]) -> Vec<ConfiguredMcpServer> {
    servers
        .iter()
        .filter_map(|server| {
            let config = server.config.as_ref()?;
            let command = config.get("command")?.as_str()?.to_owned();
            let args = config
                .get("args")
                .and_then(serde_json::Value::as_array)
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            Some(ConfiguredMcpServer { name: server.name.clone(), command, args })
        })
        .collect()
}

/// Friendly label for a known-infra process: the CONFIGURED MCP server name
/// when the process's command + args uniquely match one of `configured`,
/// else the package-derived name from [`classify_known_infra`]. `None` when
/// neither produces a name - the process isn't a recognized MCP server.
///
/// The configured match is tried FIRST because [`classify_known_infra`] only
/// knows package-naming conventions: a locally-launched script fits none of
/// them, so gating on it would drop a configured server to a generic row.
pub fn resolve_infra_label(
    cmdline: &str,
    configured: &[ConfiguredMcpServer],
) -> Option<InfraLabel> {
    let inner = extract_inner_command(cmdline).unwrap_or_else(|| cmdline.to_owned());
    let haystack = normalize_ws(&inner);
    let matched: Vec<&ConfiguredMcpServer> =
        configured.iter().filter(|server| configured_server_matches(&haystack, server)).collect();
    match matched.as_slice() {
        [only] => Some(InfraLabel { name: strip_plugin_namespace(&only.name).to_owned() }),
        [] => classify_known_infra(&inner),
        many => {
            tracing::debug!(
                candidates = ?many.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                cmdline = %cmdline,
                "MCP process matched multiple configured servers; using package name"
            );
            classify_known_infra(&inner)
        }
    }
}

/// The bare server name inside a plugin-namespaced configured name. The CLI
/// exposes a plugin-provided MCP server as `plugin:<plugin>:<server>`, which
/// reads worse than the package-derived name; anything else (including a
/// plain name that merely contains a colon) is returned verbatim.
pub fn strip_plugin_namespace(name: &str) -> &str {
    let mut parts = name.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("plugin"), Some(plugin), Some(server))
            if !plugin.is_empty() && !server.is_empty() =>
        {
            server
        }
        _ => name,
    }
}

/// One depth-0 process the elimination join may consider, with whether an
/// alive wire tool call already claims it.
#[derive(Debug, Clone, Copy)]
pub struct RootProcess<'a> {
    pub pid: u32,
    pub cmdline: &'a str,
    pub wire_matched: bool,
}

/// Executables an stdio MCP server's live process image runs under, matched on
/// the argv[0] basename so a server that `exec`ed out of its configured
/// `sh -c` wrapper still qualifies.
///
/// **No shell belongs here.** A shell is how claude spawns both its Bash-tool
/// wrapper and its `type: "command"` hooks, and both land as direct children of
/// the same pid - so whitelisting one puts every tool call and hook in the
/// candidate pool, which declines the pairing and drops the row out of the MCP
/// tier while it runs. Nothing is lost: a server that has `exec`ed is no longer
/// shell-shaped, and one that hasn't still text-matches its own `-c` script.
const INTERPRETERS: [&str; 7] = ["node", "python", "python3", "deno", "bun", "npx", "uvx"];

/// True when a root process may stand in for an unmatched server: its argv[0]
/// is one of [`INTERPRETERS`]. Keeps transient tool processes (an `rg` from a
/// Grep) out of the pool, which would otherwise decline the pairing on any scan
/// where one was alive and flicker the row.
fn is_elimination_candidate(cmdline: &str) -> bool {
    let Some(exe) = cmdline.split_whitespace().next() else {
        return false;
    };
    let base = exe.rsplit('/').next().unwrap_or(exe);
    INTERPRETERS.contains(&base)
}

/// Pair the sole unmatched `Connected` stdio server with the sole unmatched
/// candidate process, returning that process's pid and the name its row takes.
///
/// A join by ELIMINATION, not identity - nothing observable ties a server to
/// its process - so two leftovers on either side decline rather than guess.
///
/// What the `Connected` filter buys is narrow, and it is not liveness: it only
/// guarantees the name attached comes from a server the CLI last reported as
/// connected, so a `Pending` or `Failed` server can never lend its name. It
/// does NOT establish that the elected process belongs to a live server. A
/// server that connected and then died still reads `Connected` until the CLI
/// notices, and there is no liveness signal on the wire to check - so in that
/// window the sole remaining candidate can be paired with a dead server's name.
/// Declining on snapshot age would not help: in that case the snapshot is fresh
/// and still says `Connected`.
pub fn elect_unmatched_server(
    servers: &[McpServerStatus],
    configured: &[ConfiguredMcpServer],
    roots: &[RootProcess<'_>],
) -> Option<(u32, String)> {
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut candidate = None;
    let mut candidates = 0usize;
    for root in roots.iter().filter(|root| !root.wire_matched) {
        if let Some(label) = resolve_infra_label(root.cmdline, configured) {
            claimed.insert(label.name);
        } else if is_elimination_candidate(root.cmdline) {
            candidates += 1;
            candidate = Some(root.pid);
        }
    }
    if candidates != 1 {
        return None;
    }
    let mut unmatched = configured
        .iter()
        .map(|server| strip_plugin_namespace(&server.name))
        .filter(|name| !claimed.contains(*name))
        .filter(|name| {
            servers.iter().any(|server| {
                strip_plugin_namespace(&server.name) == *name
                    && server.status == forge_primitives::McpServerConnectionStatus::Connected
            })
        });
    let elected = unmatched.next()?;
    if unmatched.next().is_some() {
        return None;
    }
    Some((candidate?, elected.to_owned()))
}

/// True when `haystack` (a whitespace-normalized process cmdline) contains
/// every match token of `server`. Leading `npx`/`npm` passthrough flags
/// (e.g. `-y`) are dropped first: `npx -y <pkg>` re-execs into `npm exec
/// <pkg>` with the flag stripped, so the tokens are the package spec (the
/// first non-flag arg) plus everything after it - where the real
/// distinguisher `--cdp-endpoint <url>` lives and survives into the process
/// cmdline. Empty / all-flag args fall back to the bare `command`. Substring
/// containment tolerates the process-side `@version` suffix.
fn configured_server_matches(haystack: &str, server: &ConfiguredMcpServer) -> bool {
    let mut tokens = server.args.iter().skip_while(|arg| arg.starts_with('-')).peekable();
    if tokens.peek().is_none() {
        let command = server.command.as_str();
        return !command.is_empty() && haystack.contains(command);
    }
    tokens.all(|token| !token.is_empty() && haystack.contains(token.as_str()))
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

    /// Ground-truth wrapper captured via `ps -axww` for a backgrounded Bash
    /// whose command contains single-quotes: the wrapper re-escapes each `'`
    /// inside the eval'd command as the POSIX sequence `'"'"'` (close-quote,
    /// a double-quoted quote, reopen-quote), and the wire `raw_input.command`
    /// was `echo 'sq-marker'; sleep 40`.
    const SINGLE_QUOTE_WRAPPER: &str = r#"/bin/zsh -c source /Users/x/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true && setopt NO_EXTENDED_GLOB NO_BARE_GLOB_QUAL 2>/dev/null || true && eval 'echo '"'"'sq-marker'"'"'; sleep 40' < /dev/null && pwd -P >| /tmp/claude-ab12-cwd"#;

    fn entry(pid: u32, parent_pid: u32, command: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent_pid,
            name: "bash".to_owned(),
            command: command.to_owned(),
            memory_bytes: 1024,
        }
    }

    fn selected_pids(entries: &[ProcessEntry]) -> std::collections::BTreeSet<u32> {
        entries.iter().map(|entry| entry.pid).collect()
    }

    #[test]
    fn select_snapshot_keeps_claude_descendants_drops_siblings() {
        // claude=100; child 200 -> grandchild 300; unrelated sibling 400 under init.
        let candidates =
            vec![entry(200, 100, "cargo build"), entry(300, 200, "rustc"), entry(400, 1, "sshd")];
        assert_eq!(
            selected_pids(&select_snapshot(candidates, 100, &[])),
            [200, 300].into_iter().collect()
        );
    }

    #[test]
    fn select_snapshot_without_commands_drops_detached_bash() {
        // Reproduce-first: a backgrounded bash setsid-detached under init
        // (parent 1, not a claude descendant). With no extra_commands the
        // scan misses it entirely - the memory-less synthetic-row bug.
        let candidates = vec![entry(200, 100, "cargo build"), entry(500, 1, REAL_WRAPPER)];
        assert_eq!(
            selected_pids(&select_snapshot(candidates, 100, &[])),
            [200].into_iter().collect()
        );
    }

    #[test]
    fn select_snapshot_adopts_command_matched_detached_bash_and_subtree() {
        // Same detached bash, now with its wire command in extra_commands:
        // it AND the child it spawned join the snapshot as adopted roots;
        // the unrelated init-child stays out.
        let candidates = vec![
            entry(200, 100, "cargo build"),
            entry(500, 1, REAL_WRAPPER),
            entry(600, 500, "gh api repos/foo"),
            entry(700, 1, "sshd"),
        ];
        let extra = vec!["gh run watch 123 --exit-status".to_owned()];
        assert_eq!(
            selected_pids(&select_snapshot(candidates, 100, &extra)),
            [200, 500, 600].into_iter().collect()
        );
    }

    #[test]
    fn select_snapshot_command_match_inside_claude_tree_not_doubled() {
        // A matched bash that IS a claude descendant appears exactly once.
        let candidates = vec![entry(500, 100, REAL_WRAPPER)];
        let extra = vec!["gh run watch 123 --exit-status".to_owned()];
        let selected = select_snapshot(candidates, 100, &extra);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].pid, 500);
    }

    #[test]
    fn select_snapshot_does_not_adopt_foreign_non_wrapper_match() {
        // A foreign process whose raw cmdline substring-matches the wire
        // command but is NOT a claude bash wrapper (nothing to unwrap) must
        // not be adopted, even though the substring matches.
        let candidates = vec![
            entry(200, 100, "cargo build"),
            entry(500, 1, "gh run watch 123 --exit-status --repo busytools/forge"),
        ];
        let extra = vec!["gh run watch 123 --exit-status".to_owned()];
        assert_eq!(
            selected_pids(&select_snapshot(candidates, 100, &extra)),
            [200].into_iter().collect()
        );
    }

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
    fn extract_inner_command_unescapes_shell_single_quotes() {
        // The `'"'"'` escape the wrapper applies to each single-quote must be
        // reversed so the recovered command reads verbatim.
        assert_eq!(
            extract_inner_command(SINGLE_QUOTE_WRAPPER).as_deref(),
            Some("echo 'sq-marker'; sleep 40")
        );
    }

    #[test]
    fn extract_inner_command_terminates_at_outermost_redirect() {
        // A command that itself contains `' < /dev/null` must not terminate the
        // unwrap early: the real terminator is the OUTERMOST occurrence (the
        // eval close-quote), so unwrapping splits on the last one.
        let wrapper = format!(
            "/bin/zsh -c source /x/snap.sh 2>/dev/null || true && eval '{}' < /dev/null && pwd -P >| /tmp/claude-x-cwd",
            r#"echo '"'"'hi'"'"' < /dev/null"#
        );
        assert_eq!(extract_inner_command(&wrapper).as_deref(), Some("echo 'hi' < /dev/null"));
    }

    #[test]
    fn single_quote_command_matches_its_wrapper() {
        // Without the un-escape the `'"'"'`-vs-`'` mismatch breaks the
        // substring match, so the OS process never correlates to its wire
        // tool call (renders as a raw unmatched row + a synthetic duplicate).
        assert!(process_cmdline_matches_tool_input(
            SINGLE_QUOTE_WRAPPER,
            "echo 'sq-marker'; sleep 40"
        ));
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

        let n = classify_known_infra("npm exec @notionhq/notion-mcp-server").expect("notion");
        assert_eq!(n.name, "notion");

        // Non-MCP npm exec + unrelated processes don't classify.
        assert!(classify_known_infra("npm exec @scope/some-tool").is_none());
        assert!(classify_known_infra("rustc --crate-name forge_tui").is_none());
        assert!(classify_known_infra("node balance-transfer.mjs").is_none());
    }

    #[test]
    fn classify_known_infra_recognizes_node_launched_mcp() {
        let l = classify_known_infra("node /opt/foo-mcp-server.js").expect("foo");
        assert_eq!(l.name, "foo");
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
        // The real playwright node child folds to the same name, so the
        // same-name MCP-child dedup collapses it under the parent.
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
        let snapshot = scan(0, &[]);
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
        let _ = scan(me, &[]);
    }

    fn mcp_status(name: &str, config: Option<serde_json::Value>) -> McpServerStatus {
        McpServerStatus {
            name: name.to_owned(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        }
    }

    fn configured(name: &str, command: &str, args: &[&str]) -> ConfiguredMcpServer {
        ConfiguredMcpServer {
            name: name.to_owned(),
            command: command.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    fn playwright_pair() -> Vec<ConfiguredMcpServer> {
        // Real config shape: `npx -y @playwright/mcp@latest --cdp-endpoint <url>`.
        // npx re-execs into `npm exec ...` and STRIPS the leading `-y`, so the
        // live process (the match haystack) never carries it.
        vec![
            configured(
                "playwright",
                "npx",
                &["-y", "@playwright/mcp@latest", "--cdp-endpoint", "http://10.10.2.8:9222"],
            ),
            configured(
                "playwright-local",
                "npx",
                &["-y", "@playwright/mcp@latest", "--cdp-endpoint", "http://127.0.0.1:9222"],
            ),
        ]
    }

    #[test]
    fn configured_mcp_servers_extracts_stdio_command_and_args() {
        // Extraction is verbatim - the leading `-y` is preserved here; it's
        // only dropped at match time (npx strips it re-execing to npm exec).
        let servers = vec![mcp_status(
            "playwright",
            Some(serde_json::json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@playwright/mcp@latest", "--cdp-endpoint", "http://10.10.2.8:9222"],
                "env": {},
            })),
        )];
        assert_eq!(
            configured_mcp_servers(&servers),
            vec![configured(
                "playwright",
                "npx",
                &["-y", "@playwright/mcp@latest", "--cdp-endpoint", "http://10.10.2.8:9222"],
            )]
        );
    }

    #[test]
    fn configured_mcp_servers_skips_url_only_and_configless() {
        let servers = vec![
            // http (url-only) server has no local process to name.
            mcp_status("remote", Some(serde_json::json!({"type": "http", "url": "https://x/mcp"}))),
            // no config blob at all.
            mcp_status("bare", None),
        ];
        assert!(configured_mcp_servers(&servers).is_empty());
    }

    #[test]
    fn resolve_infra_label_uses_configured_name_for_same_package_distinct_args() {
        // Two servers on the same @playwright/mcp package, distinguished
        // only by --cdp-endpoint. Package-only naming collapses both to
        // "playwright"; the configured match keeps them distinct.
        let servers = playwright_pair();
        let hub = "npm exec @playwright/mcp@latest --cdp-endpoint http://10.10.2.8:9222";
        let local = "npm exec @playwright/mcp@latest --cdp-endpoint http://127.0.0.1:9222";
        assert_eq!(resolve_infra_label(hub, &servers).expect("hub").name, "playwright");
        assert_eq!(resolve_infra_label(local, &servers).expect("local").name, "playwright-local");
    }

    #[test]
    fn resolve_infra_label_falls_back_to_package_name_when_unmatched() {
        // A running MCP process with no configured match keeps the
        // package-derived name.
        let ctx = resolve_infra_label("npm exec @upstash/context7-mcp", &playwright_pair())
            .expect("context7");
        assert_eq!(ctx.name, "context7");
    }

    #[test]
    fn resolve_infra_label_falls_back_on_ambiguous_match() {
        // Two configured servers whose args both match one process: an
        // ambiguous tie falls back to the package name rather than guessing.
        let servers = vec![
            configured("pw-a", "npx", &["@playwright/mcp@latest"]),
            configured("pw-b", "npx", &["@playwright/mcp@latest"]),
        ];
        let label = resolve_infra_label(
            "npm exec @playwright/mcp@latest --cdp-endpoint http://10.10.2.8:9222",
            &servers,
        )
        .expect("playwright");
        assert_eq!(label.name, "playwright");
    }

    #[test]
    fn resolve_infra_label_names_local_node_script_by_configured_name() {
        // A locally-launched server fits no package-naming convention, so
        // `classify_known_infra` can't name it. The configured match must be
        // tried FIRST or the row degrades to a raw-invocation generic process.
        let servers = vec![configured("busymail", "node", &["/Users/x/busymail/dist/server.js"])];
        let label = resolve_infra_label("node /Users/x/busymail/dist/server.js", &servers)
            .expect("busymail");
        assert_eq!(label.name, "busymail");
    }

    #[test]
    fn resolve_infra_label_is_none_for_non_mcp_process() {
        // A non-MCP process stays unclassified even when servers are configured.
        assert!(resolve_infra_label("rustc --crate-name forge_tui", &playwright_pair()).is_none());
    }

    #[test]
    fn resolve_infra_label_without_configured_matches_package_name() {
        // No configured servers -> identical to classify_known_infra.
        assert_eq!(
            resolve_infra_label(
                "npm exec @playwright/mcp@latest --cdp-endpoint http://10.10.2.8:9222",
                &[],
            )
            .expect("playwright")
            .name,
            "playwright"
        );
    }

    /// The live busymail shape. Its config `exec`s a downloaded shim, so the
    /// process image shares no text with the command it launched from - the
    /// case the elimination join exists for. Captured via `ps -axww`.
    const SHIM: &str = "node /var/folders/0q/q18_z8w555v6z6q3q_3fc_t00000gn/T/tmp.9Vg/shim.mjs";

    fn root(pid: u32, cmdline: &str) -> RootProcess<'_> {
        RootProcess { pid, cmdline, wire_matched: false }
    }

    /// busymail's real config, plus the three servers that DO text-match.
    fn busymail_config() -> Vec<ConfiguredMcpServer> {
        vec![
            configured(
                "busymail",
                "sh",
                &["-c", "shim=$(mktemp -d)/shim.mjs && exec node \"$shim\""],
            ),
            configured("context7", "npx", &["-y", "@upstash/context7-mcp"]),
        ]
    }

    fn connected(names: &[&str]) -> Vec<McpServerStatus> {
        names.iter().map(|name| mcp_status(name, None)).collect()
    }

    #[test]
    fn elects_the_sole_unmatched_connected_server_for_the_sole_candidate() {
        // context7's process text-matches, so it claims its server and drops
        // out of both sides; busymail and the shim are the only leftovers.
        let elected = elect_unmatched_server(
            &connected(&["busymail", "context7"]),
            &busymail_config(),
            &[root(100, "npm exec @upstash/context7-mcp"), root(200, SHIM)],
        );
        assert_eq!(elected, Some((200, "busymail".to_owned())));
    }

    #[test]
    fn declines_when_a_second_server_is_unmatched() {
        // Two unmatched servers and one leftover process: which one the
        // process belongs to is unknowable, so no row gets named.
        let elected = elect_unmatched_server(
            &connected(&["busymail", "context7"]),
            &busymail_config(),
            &[root(200, SHIM)],
        );
        assert_eq!(elected, None);
    }

    #[test]
    fn declines_when_a_second_candidate_process_is_unmatched() {
        let elected = elect_unmatched_server(
            &connected(&["busymail", "context7"]),
            &busymail_config(),
            &[
                root(100, "npm exec @upstash/context7-mcp"),
                root(200, SHIM),
                root(300, "node /Users/x/other-service.mjs"),
            ],
        );
        assert_eq!(elected, None);
    }

    #[test]
    fn declines_a_server_that_has_not_handshaken() {
        // Without the Connected gate a crashed server would pair with any
        // leftover process - labelling an unrelated build as busymail.
        let mut servers = connected(&["busymail", "context7"]);
        servers[0].status = forge_primitives::McpServerConnectionStatus::Pending;
        let elected = elect_unmatched_server(
            &servers,
            &busymail_config(),
            &[root(100, "npm exec @upstash/context7-mcp"), root(200, SHIM)],
        );
        assert_eq!(elected, None);
    }

    #[test]
    fn a_transient_tool_process_does_not_spoil_the_election() {
        // The stability property. An `rg` from a Grep call is alive at scan
        // time and is not interpreter-shaped; counting it would decline the
        // pairing and flicker busymail's row back to a raw path every time
        // the user searched.
        let elected = elect_unmatched_server(
            &connected(&["busymail", "context7"]),
            &busymail_config(),
            &[
                root(100, "npm exec @upstash/context7-mcp"),
                root(200, SHIM),
                root(300, "rg --json -e pattern /Users/x/Projects"),
            ],
        );
        assert_eq!(elected, Some((200, "busymail".to_owned())));
    }

    #[test]
    fn a_shell_spawned_child_of_claude_does_not_spoil_the_election() {
        // Claude spawns its Bash-tool wrapper AND its `type: "command"` hooks
        // through a shell, both as direct children of the same pid - so any
        // shell in INTERPRETERS would put every tool call and hook in the pool,
        // declining the pairing and dropping the row out of the MCP tier while
        // it runs. Re-adding one fails this test.
        let wrapper = "/bin/bash -c source /Users/x/.claude/snap.sh 2>/dev/null || true && eval 'cargo build' < /dev/null && pwd -P >| /tmp/claude-ab12-cwd";
        let elected = elect_unmatched_server(
            &connected(&["busymail", "context7"]),
            &busymail_config(),
            &[root(100, "npm exec @upstash/context7-mcp"), root(200, SHIM), root(400, wrapper)],
        );
        assert_eq!(elected, Some((200, "busymail".to_owned())));
    }

    #[test]
    fn a_wire_matched_process_is_never_a_candidate() {
        // A foreground Bash already owns its row; it must not stand in for a
        // server even when its image is interpreter-shaped.
        let elected = elect_unmatched_server(
            &connected(&["busymail"]),
            &busymail_config()[..1],
            &[RootProcess { pid: 200, cmdline: "node build.mjs", wire_matched: true }],
        );
        assert_eq!(elected, None);
    }

    #[test]
    fn configured_mcp_servers_extracts_command_only_server() {
        // A stdio server with a command and no args extracts with empty args.
        let servers =
            vec![mcp_status("fs", Some(serde_json::json!({"type": "stdio", "command": "mcp-fs"})))];
        assert_eq!(configured_mcp_servers(&servers), vec![configured("fs", "mcp-fs", &[])]);
    }

    #[test]
    fn resolve_infra_label_matches_bare_command_server() {
        // A server configured as a bare command (no args) matches a process
        // whose cmdline contains that command, taking the configured name.
        let servers = vec![configured("ctx-custom", "context7-mcp", &[])];
        let label = resolve_infra_label("npm exec @upstash/context7-mcp", &servers).expect("ctx");
        assert_eq!(label.name, "ctx-custom");
    }

    #[test]
    fn resolve_infra_label_near_miss_falls_back_to_package() {
        // A single configured server on the right package but a DIFFERENT
        // --cdp-endpoint than the process: the endpoint token is absent, so
        // the every-token match fails and the package name wins. An
        // any-token match would wrongly pick "pw-hub".
        let servers = vec![configured(
            "pw-hub",
            "npx",
            &["-y", "@playwright/mcp@latest", "--cdp-endpoint", "http://10.10.2.8:9222"],
        )];
        let label = resolve_infra_label(
            "npm exec @playwright/mcp@latest --cdp-endpoint http://127.0.0.1:9222",
            &servers,
        )
        .expect("pw");
        assert_eq!(label.name, "playwright");
    }

    #[test]
    fn resolve_infra_label_strips_plugin_namespace_from_configured_name() {
        // A plugin-provided server is namespaced `plugin:<plugin>:<server>`
        // by the CLI (the `context7` plugin's `.mcp.json` names its server
        // `context7`, launched as `npx -y @upstash/context7-mcp`), so the
        // configured name alone reads worse than the package-derived one.
        let servers =
            vec![configured("plugin:context7:context7", "npx", &["-y", "@upstash/context7-mcp"])];
        assert_eq!(
            resolve_infra_label("npm exec @upstash/context7-mcp", &servers).expect("ctx").name,
            "context7",
        );
    }

    #[test]
    fn resolve_infra_label_keeps_plain_configured_name_with_a_colon() {
        // Only the `plugin:<plugin>:<server>` shape is stripped; a name that
        // merely contains a colon survives verbatim.
        let servers = vec![configured("ctx:7", "npx", &["@upstash/context7-mcp"])];
        assert_eq!(
            resolve_infra_label("npm exec @upstash/context7-mcp", &servers).expect("ctx").name,
            "ctx:7",
        );
    }

    #[test]
    fn resolve_infra_label_empty_config_does_not_match_everything() {
        // A degenerate config (empty command, no args) must not match every
        // process; it falls back to the package name.
        let servers = vec![configured("blank", "", &[])];
        let label = resolve_infra_label("npm exec @upstash/context7-mcp", &servers).expect("ctx");
        assert_eq!(label.name, "context7");
    }
}
