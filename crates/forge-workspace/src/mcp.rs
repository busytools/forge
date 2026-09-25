//! In-process MCP server forge exposes to every spawned `claude`
//! subprocess.
//!
//! The single MCP server is named `forge` and grouped by submodule:
//!
//! - `agents` - every session reaches every other by its slot: `list`,
//!   `tell`, `ask` and `whoami` for any caller, plus `spawn`,
//!   `despawn`, `update` and `capacity` for a lead. Tools render as
//!   `mcp__forge__agents__<name>`.
//! - `review` - the review-conversation loop (list / get / reply /
//!   resolve). Tools render as `mcp__forge__review__<name>`.
//! - `cron` - the caller's own durable crons.
//! - `tasks` - the caller's own project's live task list.
//! - `gotify` - the caller's own Gotify subscriptions.
//! - `slack` - the caller's own Slack subscriptions, reads and held
//!   outbound actions.
//!
//! Tool surface depends on the calling session's kind:
//!
//! - **Lead** sessions (project leads, including project sessions
//!   another agent spawned) see all eight `agents__*` verbs. A lead is
//!   the only role that can spawn, despawn, update or read capacity,
//!   because each of those acts on the caller's own project.
//! - **Worker** sessions see the four shared verbs and none of the
//!   lead-only ones. The reach is the same: a worker may address any
//!   other session by its slot, its own lead and siblings included.
//!
//! Future submodules slot in alongside these (e.g. `worktree`,
//! `memory`) without changing the server name or the auto-approve
//! fast-path in `forge-sdk::control_dispatch` (which matches the
//! `mcp__forge__` prefix at the tool-name level).

use std::sync::Arc;

use forge_sdk::mcp::server::{McpServer, McpServerBuilder};

use crate::SessionSlot;
use crate::mcp::agents::facade::AgentDispatcher;
use crate::mcp::cron::facade::CronFacade;
use crate::mcp::gotify::facade::GotifyFacade;
use crate::mcp::peers::facade::WorkspaceFacade;
use crate::mcp::review::facade::ReviewFacade;
use crate::mcp::slack::facade::SlackFacade;
use crate::mcp::tasks::facade::TasksFacade;
use crate::mcp::workers::facade::WorkerFacade;

pub mod agents;
pub(crate) mod caller_context;
pub mod cron;
pub mod gotify;
pub mod peers;
pub mod review;
pub mod slack;
pub mod tasks;
pub mod workers;

/// Identifies which kind of session the MCP server is being built
/// for. Drives the tool-surface filter in [`build_forge_server`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// Project lead - the session representing a project. Sees all
    /// eight `agents__*` verbs.
    Lead,
    /// Worker - a child agent a lead spawned. Sees the four shared
    /// `agents__*` verbs and none of the lead-only ones.
    Worker,
}

/// Build the per-session `forge` MCP server. ONE McpServer named
/// `forge` carrying the coordination tool groups appropriate for the
/// calling session's [`SessionKind`]:
///
/// - [`SessionKind::Lead`] → agents (all eight) + review + cron + tasks +
///   gotify + slack.
/// - [`SessionKind::Worker`] → agents (the shared four) + review + cron +
///   tasks + gotify + slack.
///
/// `review`, `cron`, `tasks`, `gotify` and `slack` are any-caller, so they
/// register for both kinds - unlike the four lead-only `agents__*` verbs. What
/// each acts on is scoped rather than gated on session kind: a review
/// conversation belongs to a project and branch, crons and subscriptions to
/// the caller that made them, and tasks to the caller's project. A worker is
/// exactly the session a review nudge lands on, so it needs `review__*`.
///
/// All submodules share the server name so the LLM sees a single
/// namespace (`mcp__forge__<group>__*`) and the auto-approve fast-path
/// in `forge-sdk::control_dispatch` matches every tool group with one
/// `mcp__forge__` prefix check.
///
/// Building two separate `McpServer::builder("forge")` instances and
/// pushing both into `OptionsBuilder::mcp_server` would collide on
/// the duplicate name and the CLI would reject one - so the right
/// shape is to combine the (selected) tool sets into a single
/// builder here.
///
/// `slot` is the calling session's slot. Each tool holds its own clone
/// of it, which is safe because a slot is stable across `/new` and
/// `/resume` - those swap the occupant and leave the slot alone.
pub fn build_forge_server(
    workspace_facade: Arc<dyn WorkspaceFacade>,
    worker_facade: Arc<dyn WorkerFacade>,
    review_facade: Arc<dyn ReviewFacade>,
    cron_facade: Arc<dyn CronFacade>,
    gotify_facade: Arc<dyn GotifyFacade>,
    slack_facade: Arc<dyn SlackFacade>,
    tasks_facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
    kind: SessionKind,
) -> McpServer {
    let mut builder = McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION"));
    let dispatcher = Arc::new(AgentDispatcher::new(workspace_facade, worker_facade.clone()));
    builder = agents::add_shared_tools(builder, dispatcher, slot.clone());
    if matches!(kind, SessionKind::Lead) {
        builder = agents::add_lead_tools(builder, worker_facade, slot.clone());
    }
    builder = review::add_tools(builder, review_facade, slot.clone());
    builder = cron::add_tools(builder, cron_facade, slot.clone());
    builder = gotify::add_tools(builder, gotify_facade, slot.clone());
    builder = tasks::add_tools(builder, tasks_facade, slot.clone());
    builder = slack::add_tools(builder, slack_facade, slot);
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::cron::facade::MockCronFacade;
    use crate::mcp::gotify::facade::MockGotifyFacade;
    use crate::mcp::peers::facade::MockWorkspaceFacade;
    use crate::mcp::review::facade::MockReviewFacade;
    use crate::mcp::slack::facade::MockSlackFacade;
    use crate::mcp::tasks::facade::MockTasksFacade;
    use crate::mcp::workers::facade::MockWorkerFacade;

    fn fake_key(s: &str) -> SessionSlot {
        SessionSlot::from_str_for_test(s)
    }

    fn forge_server(kind: SessionKind) -> McpServer {
        build_forge_server(
            MockWorkspaceFacade::new().into_arc(),
            MockWorkerFacade::new().into_arc(),
            MockReviewFacade::new().into_arc(),
            MockCronFacade::new().into_arc(),
            MockGotifyFacade::new().into_arc(),
            MockSlackFacade::new().into_arc(),
            MockTasksFacade::new().into_arc(),
            fake_key("test"),
            kind,
        )
    }

    /// Every tool name the server registered, read off its debug
    /// listing - which is exactly the set the LLM is offered.
    fn registered_names(kind: SessionKind) -> Vec<String> {
        let debug = format!("{:?}", forge_server(kind));
        let (_, tools) = debug.split_once("tools: [").expect("debug lists the tool names");
        let (tools, _) = tools.split_once(']').expect("the tool list is closed");
        tools
            .split(", ")
            .map(|name| name.trim_matches('"').to_owned())
            .filter(|name| !name.is_empty())
            .collect()
    }

    fn agents_tools(kind: SessionKind) -> Vec<String> {
        registered_names(kind).into_iter().filter(|name| name.starts_with("agents__")).collect()
    }

    /// The eleven names the agents family replaces. None may survive:
    /// an alias would leave two names for one verb in the shipped text,
    /// which is the confusion the merge exists to remove.
    const OLD_NAMES: [&str; 11] = [
        "peers__whoami",
        "peers__list_agents",
        "peers__tell_agent",
        "peers__ask_agent",
        "workers__spawn",
        "workers__list",
        "workers__capacity",
        "workers__tell",
        "workers__ask",
        "workers__despawn",
        "workers__update",
    ];

    /// Every group that is any-caller: both session kinds manage their
    /// own project's reviews, crons, tasks and subscriptions.
    const ANY_CALLER_TOOLS: [&str; 27] = [
        "review__list",
        "review__get",
        "review__reply",
        "review__resolve",
        "cron__create",
        "cron__list",
        "cron__delete",
        "tasks__create",
        "tasks__update",
        "tasks__list",
        "tasks__delete",
        "gotify__subscribe",
        "gotify__list",
        "gotify__unsubscribe",
        "gotify__apps",
        "gotify__recent",
        "slack__list",
        "slack__subscribe",
        "slack__unsubscribe",
        "slack__post",
        "slack__edit",
        "slack__react",
        "slack__attachment",
        "slack__search",
        "slack__user",
        "slack__pins",
        "slack__bookmarks",
    ];

    #[test]
    fn a_lead_sees_all_eight() {
        assert_eq!(
            agents_tools(SessionKind::Lead),
            [
                "agents__ask",
                "agents__capacity",
                "agents__despawn",
                "agents__list",
                "agents__spawn",
                "agents__tell",
                "agents__update",
                "agents__whoami",
            ],
        );
    }

    #[test]
    fn a_worker_sees_only_the_shared_four() {
        // The role gate: a worker gains the cross-project reach the
        // merge adds, and must NOT gain a lead-only verb with it.
        assert_eq!(
            agents_tools(SessionKind::Worker),
            ["agents__ask", "agents__list", "agents__tell", "agents__whoami"],
        );
    }

    #[test]
    fn the_eleven_old_names_are_gone() {
        for kind in [SessionKind::Lead, SessionKind::Worker] {
            let names = registered_names(kind);
            for old in OLD_NAMES {
                assert!(!names.contains(&old.to_owned()), "{old} survives for {kind:?}: {names:?}");
            }
        }
    }

    #[test]
    fn every_any_caller_group_registers_for_both_kinds() {
        for kind in [SessionKind::Lead, SessionKind::Worker] {
            let names = registered_names(kind);
            for expected in ANY_CALLER_TOOLS {
                assert!(
                    names.contains(&expected.to_owned()),
                    "{expected} must register for {kind:?}"
                );
            }
        }
    }

    /// A `replay-only:` marker exempts the retired tools it NAMES, on one
    /// of the three lines around it.
    ///
    /// Two rules, and each closes a hole the other leaves. Naming: a marker
    /// exempts only the tools written after it, so an unlisted retired name
    /// beside it still fails - a marker naming something absent is merely
    /// inert, and what it cannot do is cover a name it does not list.
    /// Adjacency: the marker must sit on the exempted line, or the line
    /// directly above or below it, so a marker cannot launder a SECOND use
    /// of the same name elsewhere in the same comment block - which is a
    /// stale instruction wearing a replay marker's clothes. Three positions
    /// rather than one because the formatter moves a trailing comment
    /// between an arm's own line and its body, and the two neighbours mean
    /// one marker covers a line either side of it.
    ///
    /// It exists for text that cites a retired name as history: a reader of
    /// what a session recorded before the rename. Nothing can call a retired
    /// tool, so the marker cannot be an alias.
    const REPLAY_ONLY: &str = "replay-only:";

    /// A line that IS one of `OLD_NAMES`' own entries. The list has to
    /// name the retired tools in order to assert them away, and this
    /// recognises exactly those lines rather than exempting the file that
    /// holds them - so a stale name added anywhere else here still fails.
    fn is_old_name_entry(line: &str) -> bool {
        let entry = line.trim().trim_end_matches(',');
        entry.len() > 1
            && entry.starts_with('"')
            && entry.ends_with('"')
            && OLD_NAMES.contains(&entry.trim_matches('"'))
    }

    /// The retired names a marker on, above or below line `at` exempts.
    fn exempted_names(lines: &[&str], at: usize) -> Vec<&'static str> {
        let adjacent = [at.checked_sub(1), Some(at), at.checked_add(1)]
            .into_iter()
            .flatten()
            .filter_map(|i| lines.get(i));
        let mut out = Vec::new();
        for line in adjacent {
            let Some((_, named)) = line.split_once(REPLAY_ONLY) else { continue };
            out.extend(OLD_NAMES.iter().copied().filter(|old| named.contains(old)));
        }
        out
    }

    /// The tracked `.rs` and `.md` files, read from the working tree, paired
    /// with how many were listed.
    ///
    /// Tracked content, not a filesystem walk. A surface forge ships is a
    /// file in the repository, and a walk descends into everything git
    /// excludes - `docs/superpowers/`, `.claude/plans/`, another
    /// worktree's checkout - so it reports a clean tree or a red one
    /// depending on who else is using the machine. The recorded baselines
    /// are `.jsonl`, so the extension filter leaves them out: a capture
    /// holds whatever the capture machine printed.
    fn tracked_source_files(root: &std::path::Path) -> (usize, Vec<(String, String)>) {
        let listed = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "-z"])
            .output()
            .expect("git ls-files runs");
        assert!(listed.status.success(), "git ls-files failed: {listed:?}");
        let listing = String::from_utf8(listed.stdout).expect("git lists UTF-8 paths");
        let paths: Vec<&str> = listing
            .split('\0')
            .filter(|rel| {
                matches!(
                    std::path::Path::new(rel).extension().and_then(|ext| ext.to_str()),
                    Some("rs" | "md")
                )
            })
            .collect();
        let listed_count = paths.len();
        let files = paths
            .into_iter()
            .filter_map(|rel| {
                std::fs::read_to_string(root.join(rel)).ok().map(|text| (rel.to_owned(), text))
            })
            .collect();
        (listed_count, files)
    }

    /// The names are gone rather than aliased, so a session that follows a
    /// stale instruction calls a tool that no longer exists, and nothing
    /// errors until it does.
    #[test]
    fn no_surface_still_names_a_retired_tool() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (listed, files) = tracked_source_files(&root);
        // Two questions, because a scan that read nothing reports the same
        // clean verdict as a scan that read everything. The count is of
        // files that READ, against what git listed, so a partial failure
        // cannot pass; the second is a floor, so an empty listing cannot.
        assert!(
            !files.is_empty() && files.len() == listed,
            "read {} of {listed} tracked .rs/.md files, so the scan is not the tree it claims",
            files.len(),
        );
        assert!(
            files.len() > 400,
            "the scan read only {} files, too few for its verdict to mean anything",
            files.len(),
        );
        let mut offenders = Vec::new();
        for (path, text) in files {
            let lines: Vec<&str> = text.lines().collect();
            for (number, line) in lines.iter().enumerate() {
                let exempt = exempted_names(&lines, number);
                let unexempted: Vec<&str> = OLD_NAMES
                    .iter()
                    .copied()
                    .filter(|old| line.contains(old) && !exempt.contains(old))
                    .collect();
                if !unexempted.is_empty() && !is_old_name_entry(line) {
                    offenders.push(format!("{path}:{} names {unexempted:?}", number + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a retired tool name survives at: {offenders:?}. If the line cites one as history \
             rather than calling it, put a `{REPLAY_ONLY}` comment naming that tool on the line \
             itself or the one next to it.",
        );
    }

    /// The exemption has to REJECT, not only accept. Without this the helper
    /// could return every retired name unconditionally - or scan the whole
    /// file rather than the adjacent lines - and the gate would pass while
    /// exempting everything, which is how the pre-rewrite version went wrong
    /// when it skipped the file outright.
    ///
    /// The fixture is built from `OLD_NAMES` rather than repeating the names,
    /// so it cannot drift from what the gate scans for.
    #[test]
    fn the_replay_exemption_names_its_tools_and_only_those() {
        let marker_line = |tool: &str| format!("// {REPLAY_ONLY} {tool}");
        let used_line = |tool: &str| format!("\"mcp__forge__{tool}\",");
        let (wrapped, other) = (OLD_NAMES[9], OLD_NAMES[3]);
        let (mark_wrapped, use_wrapped) = (marker_line(wrapped), used_line(wrapped));
        let (mark_other, use_other) = (marker_line(other), used_line(other));
        let lines = [
            mark_wrapped.as_str(),
            use_wrapped.as_str(),
            "",
            mark_wrapped.as_str(),
            use_other.as_str(),
            "",
            mark_other.as_str(),
            "",
            "",
            use_other.as_str(),
        ];
        assert_eq!(exempted_names(&lines, 1), vec![wrapped], "a marker exempts the tool it names");
        assert!(
            !exempted_names(&lines, 4).contains(&other),
            "a marker naming another tool does not cover this line's tool",
        );
        assert!(
            exempted_names(&lines, 9).is_empty(),
            "a marker three lines away is not adjacent, whatever it names",
        );
        assert!(
            exempted_names(&[use_wrapped.as_str(), "// nothing here"], 0).is_empty(),
            "an unmarked line is exempt from nothing",
        );
    }

    #[test]
    fn the_server_is_named_forge_for_every_session_kind() {
        // The SDK auto-approve fast-path matches `mcp__forge__` once, so
        // every group has to share the server name.
        for kind in [SessionKind::Lead, SessionKind::Worker] {
            let debug = format!("{:?}", forge_server(kind));
            assert!(
                debug.contains("name: \"forge\""),
                "server name must be 'forge'; debug: {debug}"
            );
        }
    }
}
