//! View-model for the Inspector pane's `MCP SERVERS` section.
//!
//! Sourced entirely from the session's MCP snapshot (`App::mcp`), so
//! every configured server renders regardless of transport or state:
//! an sdk/in-process server has no process at all and a pending or
//! failed server has no handshake - both render here, where the OS
//! process walk could only ever show servers with a live pid. The
//! walk contributes the optional process line under a subprocess-
//! backed server, joined through the same machinery the old
//! PROCESSES MCP tier used.

use std::collections::{HashMap, HashSet};

use forge_primitives::{McpServerConnectionStatus, McpServerStatus};
use forge_workspace::env::processes::{
    ProcessEntry, ProcessSnapshot, RootProcess, basename_exe, configured_mcp_servers,
    configured_text_match, elect_unmatched_server, strip_plugin_namespace,
};
use serde_json::Value;

use super::App;

/// The process line under a subprocess-backed server: the backing
/// command, its subtree's resident memory, and the pid.
#[derive(Debug, PartialEq, Eq)]
pub struct McpProcessLine {
    pub command: String,
    pub memory_bytes: u64,
    pub pid: u32,
}

/// One row in the MCP SERVERS section.
#[derive(Debug, PartialEq, Eq)]
pub struct McpServerRow {
    /// Server name as the user knows it - plugin namespaces stripped
    /// (`plugin:context7:context7` renders as `context7`).
    pub name: String,
    pub status: McpServerConnectionStatus,
    /// Second line under the name: scope and either the tool count
    /// (connected), `pending`, or the failure reason.
    pub detail: String,
    /// Present only when the snapshot's OS walk matched a process to
    /// this server (stdio transports).
    pub process: Option<McpProcessLine>,
}

/// The section's rows plus the pids its join claimed from the OS
/// snapshot. `collect_active_processes` skips those processes so a
/// server's backing tree renders exactly once - here, not in
/// PROCESSES.
#[derive(Debug)]
pub struct McpServerSection {
    pub rows: Vec<McpServerRow>,
    pub claimed_pids: HashSet<u32>,
}

/// Collect the MCP SERVERS rows for the active session, ordered
/// connected first by name, then pending, then failed.
pub fn collect_mcp_servers(app: &App) -> McpServerSection {
    let Some(session) = app.active_session() else {
        return McpServerSection { rows: Vec::new(), claimed_pids: HashSet::new() };
    };
    let servers = &app.mcp().servers;
    if servers.is_empty() {
        return McpServerSection { rows: Vec::new(), claimed_pids: HashSet::new() };
    }

    let mut process_of: HashMap<String, (&ProcessEntry, u64)> = HashMap::new();
    let mut claimed_pids: HashSet<u32> = HashSet::new();
    if let Some(snapshot) = session.process_snapshot.as_ref() {
        join_processes_to_servers(
            snapshot,
            servers,
            &crate::app::processes::wire_alive_tool_calls(session),
            &mut process_of,
            &mut claimed_pids,
        );
    }

    let mut rows: Vec<McpServerRow> = servers
        .iter()
        .map(|server| {
            let name = strip_plugin_namespace(&server.name);
            McpServerRow {
                name: name.to_owned(),
                status: server.status,
                detail: detail_line(server),
                process: process_of.get(name).map(|(entry, memory_bytes)| McpProcessLine {
                    command: basename_exe(entry.command.trim()),
                    memory_bytes: *memory_bytes,
                    pid: entry.pid,
                }),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        status_rank(a.status).cmp(&status_rank(b.status)).then_with(|| a.name.cmp(&b.name))
    });
    McpServerSection { rows, claimed_pids }
}

/// Join each snapshot server to at most one OS process: by configured
/// command + args text first, then by the elimination election for a
/// server whose process shares no text with its config. Records the
/// winner per stripped server name plus every pid its subtree claims.
fn join_processes_to_servers<'a>(
    snapshot: &'a ProcessSnapshot,
    servers: &[McpServerStatus],
    wire_alive: &[&crate::app::state::tool_call_info::ToolCallInfo],
    process_of: &mut HashMap<String, (&'a ProcessEntry, u64)>,
    claimed_pids: &mut HashSet<u32>,
) {
    let configured = configured_mcp_servers(servers);
    let by_pid: HashMap<u32, &ProcessEntry> =
        snapshot.processes.iter().map(|e| (e.pid, e)).collect();
    let mut children_of: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    for entry in &snapshot.processes {
        children_of.entry(entry.parent_pid).or_default().push(entry);
    }

    // Per-entry claims on configured TEXT evidence only. A process that
    // matches no configured command - or matches several - stays
    // unclaimed: the section is snapshot-sourced, and a package-derived
    // name for a server the snapshot does not list is guessing by
    // construction. Unclaimed processes keep rendering in PROCESSES.
    let mut candidates: HashMap<String, Vec<u32>> = HashMap::new();
    for entry in &snapshot.processes {
        if crate::app::processes::wire_match(entry, wire_alive).is_some() {
            continue;
        }
        if let Some(server) = configured_text_match(&entry.command, &configured) {
            candidates
                .entry(strip_plugin_namespace(&server.name).to_owned())
                .or_default()
                .push(entry.pid);
        }
    }

    // A server whose process shares no text with its configured command
    // can still be named when it is the only leftover on both sides.
    // Computed over the roots, since the decision is global.
    let roots: Vec<RootProcess<'_>> = snapshot
        .processes
        .iter()
        .filter(|e| !by_pid.contains_key(&e.parent_pid))
        .map(|root| RootProcess {
            pid: root.pid,
            cmdline: root.command.as_str(),
            wire_matched: crate::app::processes::wire_match(root, wire_alive).is_some(),
        })
        .collect();
    if let Some((pid, name)) = elect_unmatched_server(servers, &configured, &roots) {
        candidates.entry(name).or_default().push(pid);
    }

    for (name, pids) in candidates {
        let Some(chosen) =
            pick_ancestor(&pids, &children_of).and_then(|pid| by_pid.get(&pid).copied())
        else {
            continue;
        };
        let memory = subtree_bytes(chosen, &children_of);
        process_of.insert(name, (chosen, memory));
        claim_subtree(chosen.pid, &children_of, claimed_pids);
    }
}

/// Resident memory of `root` plus all its descendants. The snapshot is
/// a tree so each pid appears once; the `visited` set guards a
/// pathological parent-chain cycle.
fn subtree_bytes(root: &ProcessEntry, children_of: &HashMap<u32, Vec<&ProcessEntry>>) -> u64 {
    fn visit(
        entry: &ProcessEntry,
        children_of: &HashMap<u32, Vec<&ProcessEntry>>,
        visited: &mut HashSet<u32>,
    ) -> u64 {
        if !visited.insert(entry.pid) {
            return 0;
        }
        let mut total = entry.memory_bytes;
        if let Some(kids) = children_of.get(&entry.pid) {
            for kid in kids {
                total = total.saturating_add(visit(kid, children_of, visited));
            }
        }
        total
    }
    let mut visited = HashSet::new();
    visit(root, children_of, &mut visited)
}

/// The one candidate that reaches all the others (the `npm exec`
/// parent over its collapsed `node` child). `None` when the candidates
/// are disjoint siblings - picking one would be snapshot-order luck, so
/// the bucket claims nothing and every candidate keeps rendering in
/// PROCESSES.
fn pick_ancestor(pids: &[u32], children_of: &HashMap<u32, Vec<&ProcessEntry>>) -> Option<u32> {
    pids.iter().copied().find(|&pid| {
        let mut reachable = HashSet::new();
        collect_descendants(pid, children_of, &mut reachable);
        pids.iter().all(|&other| other == pid || reachable.contains(&other))
    })
}

fn claim_subtree(
    pid: u32,
    children_of: &HashMap<u32, Vec<&ProcessEntry>>,
    claimed: &mut HashSet<u32>,
) {
    claimed.insert(pid);
    if let Some(kids) = children_of.get(&pid) {
        for kid in kids {
            claim_subtree(kid.pid, children_of, claimed);
        }
    }
}

fn collect_descendants(
    pid: u32,
    children_of: &HashMap<u32, Vec<&ProcessEntry>>,
    out: &mut HashSet<u32>,
) {
    if let Some(kids) = children_of.get(&pid) {
        for kid in kids {
            if out.insert(kid.pid) {
                collect_descendants(kid.pid, children_of, out);
            }
        }
    }
}

/// Sort rank: connected, then pending, then every not-usable state.
/// NeedsAuth / Disabled never appear in the --print snapshot today
/// (disabled servers are dropped from the response entirely); they
/// rank with failed and say so on the detail line.
fn status_rank(status: McpServerConnectionStatus) -> u8 {
    match status {
        McpServerConnectionStatus::Connected => 0,
        McpServerConnectionStatus::Pending => 1,
        McpServerConnectionStatus::Failed
        | McpServerConnectionStatus::NeedsAuth
        | McpServerConnectionStatus::Disabled => 2,
    }
}

/// The detail line: scope, then either the tool count (connected),
/// `pending`, or the failure reason.
fn detail_line(server: &McpServerStatus) -> String {
    let mut parts = vec![scope_label(server)];
    match server.status {
        McpServerConnectionStatus::Connected => {
            if let Some(tools) = server.tools.as_ref() {
                parts.push(crate::ui::format::tool_summary(tools.len()));
            }
        }
        McpServerConnectionStatus::Pending => parts.push("pending".to_owned()),
        McpServerConnectionStatus::Failed => parts.push(
            server
                .error
                .as_deref()
                .filter(|error| !error.trim().is_empty())
                .unwrap_or("failed")
                .to_owned(),
        ),
        McpServerConnectionStatus::NeedsAuth => parts.push("needs auth".to_owned()),
        McpServerConnectionStatus::Disabled => parts.push("disabled".to_owned()),
    }
    parts.join(" \u{00B7} ")
}

/// Scope as the /mcp view shows it, with the sdk fallback an in-process
/// server needs: it has no configured scope, and `sdk` is what its
/// config blob says it is.
fn scope_label(server: &McpServerStatus) -> String {
    if let Some(scope) = server.scope.as_deref() {
        return scope.to_owned();
    }
    match server.config.as_ref().and_then(|config| config.get("type")).and_then(Value::as_str) {
        Some("sdk") => "sdk".to_owned(),
        _ => "session".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::McpToolInfo;
    use forge_workspace::env::processes::ProcessEntry;

    use crate::app::ChatMessage;
    use crate::app::MessageBlock;
    use crate::app::MessageRole;
    use crate::app::state::tool_call_info::ToolCallInfo;

    fn server(name: &str, status: McpServerConnectionStatus) -> McpServerStatus {
        McpServerStatus { name: name.to_owned(), status, ..McpServerStatus::default() }
    }

    /// A connected stdio server with a config the process matcher can
    /// join on, plus the handshake facts a connected server carries.
    fn connected_stdio(name: &str, command: &str, args: &[&str], tools: usize) -> McpServerStatus {
        McpServerStatus {
            status: McpServerConnectionStatus::Connected,
            config: Some(serde_json::json!({
                "type": "stdio",
                "command": command,
                "args": args,
            })),
            scope: Some("user".to_owned()),
            tools: Some(
                (0..tools)
                    .map(|i| McpToolInfo {
                        name: format!("t{i}"),
                        description: None,
                        annotations: None,
                    })
                    .collect(),
            ),
            ..server(name, McpServerConnectionStatus::Connected)
        }
    }

    fn entry(pid: u32, parent_pid: u32, command: &str, memory_bytes: u64) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent_pid,
            name: "npm".to_owned(),
            command: command.to_owned(),
            memory_bytes,
        }
    }

    fn npm_snapshot() -> ProcessSnapshot {
        // Live shape: the npm parent and its collapsed node child both
        // classify to the same server.
        ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                entry(300, 1, "npm exec @upstash/context7-mcp", 81 * 1024 * 1024),
                entry(301, 300, "node /x/.bin/context7-mcp", 81 * 1024 * 1024),
            ],
        }
    }

    fn app_with(servers: Vec<McpServerStatus>, snapshot: Option<ProcessSnapshot>) -> App {
        let mut app = App::test_default();
        if let Some(snapshot) = snapshot {
            app.set_active_process_snapshot_for_test(snapshot);
        }
        app.mcp_mut().servers = servers;
        app
    }

    #[test]
    fn every_snapshot_server_renders_including_a_process_free_sdk_one() {
        // The regression this section closes: an sdk/in-process server
        // has no pid for the OS walk to find, so the old PROCESSES tier
        // silently dropped it. Snapshot-sourced means it renders with
        // or without a process snapshot at all.
        let forge = McpServerStatus {
            config: Some(serde_json::json!({ "type": "sdk", "name": "forge" })),
            tools: Some(
                (0..18)
                    .map(|i| McpToolInfo {
                        name: format!("t{i}"),
                        description: None,
                        annotations: None,
                    })
                    .collect(),
            ),
            ..server("forge", McpServerConnectionStatus::Connected)
        };
        let playwright = connected_stdio("playwright", "npx", &["-y", "@playwright/mcp"], 24);
        let app = app_with(vec![forge, playwright], None);

        let section = collect_mcp_servers(&app);
        assert_eq!(section.rows.len(), 2, "got {section:?}");
        let forge_row = section.rows.iter().find(|r| r.name == "forge").expect("forge row");
        assert_eq!(forge_row.detail, "sdk \u{00B7} 18 tools");
        assert_eq!(forge_row.process, None, "an sdk server has no process");
        assert!(section.claimed_pids.is_empty());
    }

    #[test]
    fn a_recognized_process_with_no_configured_claim_stays_in_processes() {
        // The reviewer's shape: `npm exec @upstash/context7-mcp` running
        // while NO configured server is named context7 (here: airmail,
        // whose wrapper script shares no text with the process). The
        // package name the old resolution derived would claim the
        // process into a void - no row carries `context7` - so the join
        // claims on configured text evidence only and the process keeps
        // rendering in PROCESSES.
        let servers = vec![McpServerStatus {
            config: Some(serde_json::json!({
                "type": "stdio",
                "command": "sh",
                "args": ["-c", "wrapper=$(mktemp -d)/w.mjs && exec node \"$wrapper\""],
            })),
            scope: Some("user".to_owned()),
            ..server("airmail", McpServerConnectionStatus::Connected)
        }];
        let app = app_with(
            servers,
            Some(ProcessSnapshot {
                scanned_at: std::time::SystemTime::now(),
                processes: vec![entry(500, 1, "npm exec @upstash/context7-mcp", 46 * 1024 * 1024)],
            }),
        );

        let section = collect_mcp_servers(&app);
        assert!(section.rows[0].process.is_none(), "no text match, no claim; got {section:?}");
        assert!(section.claimed_pids.is_empty(), "got {:?}", section.claimed_pids);
        let coll = crate::app::processes::collect_active_processes(&app, &section.claimed_pids);
        assert_eq!(
            coll.rows.iter().map(|r| r.headline.as_str()).collect::<Vec<_>>(),
            vec!["npm exec @upstash/context7-mcp"],
            "the process keeps rendering in PROCESSES; got {:?}",
            coll.rows,
        );
    }

    #[test]
    fn sibling_processes_matching_one_server_claim_nothing() {
        // Subset-args shape: a bare `npm exec <pkg>` beside the full
        // `npm exec <pkg> --cdp-endpoint ...` both text-match a config
        // that carries no distinguishing args. One bucket, disjoint
        // siblings - picking either would be snapshot-order luck, so the
        // bucket claims nothing and both keep rendering in PROCESSES.
        let servers =
            vec![connected_stdio("playwright", "npx", &["-y", "@playwright/mcp@latest"], 24)];
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                entry(200, 1, "npm exec @playwright/mcp@latest", 40 * 1024 * 1024),
                entry(
                    300,
                    1,
                    "npm exec @playwright/mcp@latest --cdp-endpoint http://127.0.0.1:9222",
                    40 * 1024 * 1024,
                ),
            ],
        };
        let app = app_with(servers, Some(snapshot));

        let section = collect_mcp_servers(&app);
        assert!(section.rows[0].process.is_none(), "siblings decline; got {section:?}");
        assert!(section.claimed_pids.is_empty(), "got {:?}", section.claimed_pids);
        let coll = crate::app::processes::collect_active_processes(&app, &section.claimed_pids);
        assert_eq!(coll.rows.len(), 2, "both siblings stay in PROCESSES; got {:?}", coll.rows);
    }

    #[test]
    fn pending_and_failed_servers_render_their_state_on_the_detail_line() {
        // Wire shapes from the live mcp_status baseline: pending with a
        // scope, failed with the CLI's error text.
        let mut pending = server("greptile", McpServerConnectionStatus::Pending);
        pending.scope = Some("user".to_owned());
        let mut failed = server("jetbrains", McpServerConnectionStatus::Failed);
        failed.error = Some("SSE error: Non-200 status code (502)".to_owned());
        failed.scope = Some("project".to_owned());
        let app = app_with(vec![pending, failed], None);

        let section = collect_mcp_servers(&app);
        let greptile = section.rows.iter().find(|r| r.name == "greptile").expect("pending row");
        assert_eq!(greptile.status, McpServerConnectionStatus::Pending);
        assert_eq!(greptile.detail, "user \u{00B7} pending");
        let jetbrains = section.rows.iter().find(|r| r.name == "jetbrains").expect("failed row");
        assert_eq!(jetbrains.detail, "project \u{00B7} SSE error: Non-200 status code (502)");
    }

    #[test]
    fn connected_first_by_name_then_pending_then_failed() {
        // Input order is the CLI's; the section's order is connected by
        // name, pending, failed.
        let mut failed = server("aaa", McpServerConnectionStatus::Failed);
        failed.scope = Some("user".to_owned());
        let mut pending = server("bbb", McpServerConnectionStatus::Pending);
        pending.scope = Some("user".to_owned());
        let zebra = connected_stdio("zebra", "npx", &["@z"], 1);
        let alpha = connected_stdio("alpha", "npx", &["@a"], 1);
        let app = app_with(vec![failed, pending, zebra, alpha], None);

        let section = collect_mcp_servers(&app);
        assert_eq!(
            section.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zebra", "bbb", "aaa"],
            "connected by name, then pending, then failed; got {section:?}",
        );
    }

    #[test]
    fn subprocess_backed_server_carries_one_process_line_with_subtree_memory() {
        // The npm parent and its collapsed node child both classify to
        // the server; the row takes the ANCESTOR (the surviving row of
        // the old tier) and its SUBTREE memory - own-RSS would silently
        // drop the collapsed leaf.
        let servers = vec![connected_stdio("context7", "npx", &["-y", "@upstash/context7-mcp"], 2)];
        let app = app_with(servers, Some(npm_snapshot()));

        let section = collect_mcp_servers(&app);
        assert_eq!(section.rows.len(), 1);
        let process = section.rows[0].process.as_ref().expect("process line");
        assert_eq!(process.command, "npm exec @upstash/context7-mcp");
        assert_eq!(process.memory_bytes, 162 * 1024 * 1024, "subtree total, not own RSS");
        assert_eq!(process.pid, 300);
        assert_eq!(section.claimed_pids, [300, 301].into_iter().collect());
    }

    #[test]
    fn the_elimination_join_names_the_sole_unpaired_server() {
        // airmail's shim execs, so its process shares no text with its
        // configured command; context7's process matches, leaving one
        // server and one interpreter-shaped process unpaired - the shim
        // takes airmail's name.
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                entry(100, 1, "npm exec @upstash/context7-mcp", 86 * 1024 * 1024),
                entry(200, 1, "node /var/folders/0q/q18/T/tmp.9Vg/shim.mjs", 90 * 1024 * 1024),
            ],
        };
        let servers = vec![
            McpServerStatus {
                config: Some(serde_json::json!({
                    "type": "stdio",
                    "command": "sh",
                    "args": ["-c", "shim=$(mktemp -d)/shim.mjs && exec node \"$shim\""],
                })),
                scope: Some("user".to_owned()),
                ..server("airmail", McpServerConnectionStatus::Connected)
            },
            connected_stdio("context7", "npx", &["-y", "@upstash/context7-mcp"], 2),
        ];
        let app = app_with(servers, Some(snapshot));

        let section = collect_mcp_servers(&app);
        let airmail = section.rows.iter().find(|r| r.name == "airmail").expect("elected row");
        let process = airmail.process.as_ref().expect("shim process joins airmail");
        assert_eq!(process.pid, 200);
    }

    #[test]
    fn the_join_declines_when_two_servers_are_left_unpaired() {
        // Two unpaired servers against one leftover process: naming the
        // shim would be a guess, so neither server gets a process line
        // and the shim stays unclaimed (it keeps rendering in
        // PROCESSES).
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![entry(
                200,
                1,
                "node /var/folders/0q/q18/T/tmp.9Vg/shim.mjs",
                90 * 1024 * 1024,
            )],
        };
        let servers = vec![
            McpServerStatus {
                config: Some(serde_json::json!({
                    "type": "stdio",
                    "command": "sh",
                    "args": ["-c", "shim=$(mktemp -d)/shim.mjs && exec node \"$shim\""],
                })),
                scope: Some("user".to_owned()),
                ..server("airmail", McpServerConnectionStatus::Connected)
            },
            connected_stdio("context7", "npx", &["-y", "@upstash/context7-mcp"], 2),
        ];
        let app = app_with(servers, Some(snapshot));

        let section = collect_mcp_servers(&app);
        assert!(section.rows.iter().all(|r| r.process.is_none()), "got {section:?}");
        assert!(section.claimed_pids.is_empty());
    }

    #[test]
    fn same_package_servers_join_by_their_configured_text() {
        // Two playwright servers on the same package, distinguished only
        // by --cdp-endpoint. Each joins its OWN process, not its
        // sibling's.
        let snapshot = ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                entry(
                    200,
                    1,
                    "npm exec @playwright/mcp@latest --cdp-endpoint http://192.0.2.10:9222",
                    40 * 1024 * 1024,
                ),
                entry(
                    201,
                    200,
                    "node /x/.bin/playwright-mcp --cdp-endpoint http://192.0.2.10:9222",
                    40 * 1024 * 1024,
                ),
                entry(
                    300,
                    1,
                    "npm exec @playwright/mcp@latest --cdp-endpoint http://127.0.0.1:9222",
                    40 * 1024 * 1024,
                ),
                entry(
                    301,
                    300,
                    "node /x/.bin/playwright-mcp --cdp-endpoint http://127.0.0.1:9222",
                    40 * 1024 * 1024,
                ),
            ],
        };
        let servers = vec![
            connected_stdio(
                "playwright",
                "npx",
                &["-y", "@playwright/mcp@latest", "--cdp-endpoint", "http://192.0.2.10:9222"],
                24,
            ),
            connected_stdio(
                "playwright-local",
                "npx",
                &["-y", "@playwright/mcp@latest", "--cdp-endpoint", "http://127.0.0.1:9222"],
                24,
            ),
        ];
        let app = app_with(servers, Some(snapshot));

        let section = collect_mcp_servers(&app);
        let by_pid: std::collections::HashMap<u32, &str> = section
            .rows
            .iter()
            .filter_map(|r| r.process.as_ref().map(|p| (p.pid, r.name.as_str())))
            .collect();
        assert_eq!(by_pid.get(&200), Some(&"playwright"));
        assert_eq!(by_pid.get(&300), Some(&"playwright-local"));
        assert_eq!(section.claimed_pids.len(), 4, "both backing subtrees claimed");
    }

    #[test]
    fn plugin_namespaced_servers_join_and_render_by_bare_name() {
        // The CLI exposes a plugin-provided server as
        // `plugin:context7:context7`; the row shows the bare name and
        // still joins its process.
        let servers = vec![McpServerStatus {
            name: "plugin:context7:context7".to_owned(),
            ..connected_stdio("context7", "npx", &["-y", "@upstash/context7-mcp"], 2)
        }];
        let app = app_with(servers, Some(npm_snapshot()));

        let section = collect_mcp_servers(&app);
        assert_eq!(section.rows[0].name, "context7");
        assert!(
            section.rows[0].process.is_some(),
            "namespaced server still joins; got {section:?}"
        );
    }

    #[test]
    fn a_wire_matched_process_is_never_claimed_as_a_server() {
        // A foreground bash running `npm exec @upstash/context7-mcp` is
        // tracked work, not the server's backing process - the join
        // must leave it unclaimed so it keeps rendering in PROCESSES.
        let mut app = app_with(
            vec![connected_stdio("context7", "npx", &["-y", "@upstash/context7-mcp"], 2)],
            Some(ProcessSnapshot {
                scanned_at: std::time::SystemTime::now(),
                processes: vec![entry(
                    400,
                    1,
                    "/bin/zsh -c -l eval 'npm exec @upstash/context7-mcp'",
                    8 * 1024 * 1024,
                )],
            }),
        );
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(wire_bash("npm exec @upstash/context7-mcp")))],
        ));

        let section = collect_mcp_servers(&app);
        assert!(section.rows[0].process.is_none(), "got {section:?}");
        assert!(section.claimed_pids.is_empty());
    }

    /// Minimal in-progress Bash tool call carrying just the fields the
    /// wire matcher reads.
    fn wire_bash(command: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: "tu-bash".to_owned(),
            title: String::new(),
            sdk_tool_name: "Bash".to_owned(),
            raw_input: Some(serde_json::json!({ "command": command })),
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: crate::agent::model::ToolCallStatus::InProgress,
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
}
