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
//! - `systemone` - the decision tools (`ask_noul`, `ask_choice`,
//!   `ask_score`), injected only when `[systemone]` is configured and
//!   enabled.
//!
//! Tool surface depends on the calling session's kind:
//!
//! - **Lead** sessions (project leads, including project sessions
//!   another agent spawned) see all seven `agents__*` verbs. A lead is
//!   the only role that can spawn, despawn, update or read capacity,
//!   because each of those acts on the caller's own project.
//! - **Worker** sessions see the three shared verbs and none of the
//!   lead-only ones. The reach is the same: a worker may address any
//!   other session by its slot, its own lead and siblings included.
//!
//! Future submodules slot in alongside these (e.g. `worktree`,
//! `memory`) without changing the server name or the auto-approve
//! fast-path in `forge-sdk::control_dispatch` (which matches the
//! `mcp__forge__` prefix at the tool-name level).

use std::collections::BTreeSet;
use std::sync::Arc;

use forge_sdk::mcp::server::{McpServer, McpServerBuilder};

use crate::SessionSlot;
use crate::mcp::agents::facade::AgentDispatcher;
use crate::mcp::cron::facade::CronFacade;
use crate::mcp::gotify::facade::GotifyFacade;
use crate::mcp::peers::facade::WorkspaceFacade;
use crate::mcp::review::facade::ReviewFacade;
use crate::mcp::slack::facade::SlackFacade;
use crate::mcp::systemone::facade::SystemOneFacade;
use crate::mcp::tasks::facade::TasksFacade;
use crate::mcp::workers::facade::WorkerFacade;

pub mod agents;
pub(crate) mod caller_context;
pub mod cron;
pub mod gotify;
pub mod peers;
pub mod review;
pub mod slack;
pub mod systemone;
pub mod tasks;
pub mod workers;

/// Identifies which kind of session the MCP server is being built
/// for. Drives the tool-surface filter in [`build_forge_server`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// Project lead - the session representing a project. Sees all
    /// seven `agents__*` verbs.
    Lead,
    /// Worker - a child agent a lead spawned. Sees the three shared
    /// `agents__*` verbs and none of the lead-only ones.
    Worker,
}

/// The toggleable MCP families. `agents` is always on and is not here:
/// a worker without `tell`/`ask` cannot report to its lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum McpFamily {
    Review,
    Cron,
    Tasks,
    Gotify,
    Slack,
    Systemone,
}

impl McpFamily {
    /// Every toggleable family, in wire order.
    pub const ALL: [McpFamily; 6] = [
        McpFamily::Review,
        McpFamily::Cron,
        McpFamily::Tasks,
        McpFamily::Gotify,
        McpFamily::Slack,
        McpFamily::Systemone,
    ];

    /// The default set: a worker whose row names none gets everything.
    pub fn all() -> BTreeSet<McpFamily> {
        Self::ALL.into_iter().collect()
    }

    /// The wire spelling, also the family segment its tool names carry.
    pub fn as_str(self) -> &'static str {
        match self {
            McpFamily::Review => "review",
            McpFamily::Cron => "cron",
            McpFamily::Tasks => "tasks",
            McpFamily::Gotify => "gotify",
            McpFamily::Slack => "slack",
            McpFamily::Systemone => "systemone",
        }
    }

    /// Parse a spawn argument's spelling; `None` for anything else.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|family| family.as_str() == name)
    }
}

/// A stored selection's resolved set: absent or empty means every
/// family; names are canonicalised into wire order and de-duplicated.
/// Unknown names cannot be written by either tool (both validate), but a
/// hand-edited row could carry one - it is dropped loudly rather than
/// widening or narrowing the surface silently.
pub(crate) fn resolve_mcp_families(stored: Option<&[String]>) -> BTreeSet<McpFamily> {
    let Some(stored) = stored else {
        return McpFamily::all();
    };
    if stored.is_empty() {
        return McpFamily::all();
    }
    let requested: BTreeSet<McpFamily> = stored
        .iter()
        .filter_map(|name| {
            let parsed = McpFamily::parse(name);
            if parsed.is_none() {
                tracing::warn!(
                    target: "forge_workspace::mcp",
                    event_name = "unknown_mcp_family_in_row",
                    name = %name,
                    "a stored mcp family name is not selectable; ignoring it",
                );
            }
            parsed
        })
        .collect();
    requested
}

/// The canonical stored form of a selection: names in wire order,
/// de-duplicated; an empty list means every family, so it is stored as
/// `None`.
pub(crate) fn canonical_mcp_families(names: &[String]) -> Result<Option<Vec<String>>, String> {
    if names.is_empty() {
        return Ok(None);
    }
    let mut selected: BTreeSet<McpFamily> = BTreeSet::new();
    for name in names {
        if name == "agents" {
            return Err("`agents` is always on for every worker and cannot be listed".to_owned());
        }
        let Some(family) = McpFamily::parse(name) else {
            let selectable: Vec<&str> =
                McpFamily::ALL.iter().map(|family| family.as_str()).collect();
            return Err(format!(
                "unknown MCP family `{name}`; the selectable families are: {}",
                selectable.join(", ")
            ));
        };
        selected.insert(family);
    }
    Ok(Some(selected.into_iter().map(McpFamily::as_str).map(str::to_owned).collect()))
}

/// The facades one session's server is composed from: the agents pair
/// plus one per toggleable family.
pub struct ForgeServerFacades {
    pub workspace: Arc<dyn WorkspaceFacade>,
    pub worker: Arc<dyn WorkerFacade>,
    pub review: Arc<dyn ReviewFacade>,
    pub cron: Arc<dyn CronFacade>,
    pub gotify: Arc<dyn GotifyFacade>,
    pub slack: Arc<dyn SlackFacade>,
    pub tasks: Arc<dyn TasksFacade>,
    pub systemone: Option<Arc<dyn SystemOneFacade>>,
}

/// Build the per-session `forge` MCP server. ONE McpServer named
/// `forge` carrying the coordination tool groups appropriate for the
/// calling session's [`SessionKind`]:
///
/// - [`SessionKind::Lead`] → agents (all seven) + the selected families.
/// - [`SessionKind::Worker`] → agents (the shared three) + the selected
///   families.
///
/// `families` narrows the toggleable groups: only the selected ones
/// register. The `agents` core is unconditional, and `systemone` is
/// doubly gated - the family and a configured client.
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
    facades: ForgeServerFacades,
    families: &BTreeSet<McpFamily>,
    slot: SessionSlot,
    kind: SessionKind,
) -> McpServer {
    let ForgeServerFacades { workspace, worker, review, cron, gotify, slack, tasks, systemone } =
        facades;
    let mut builder = McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION"));
    let dispatcher = Arc::new(AgentDispatcher::new(workspace, worker.clone()));
    builder = agents::add_shared_tools(builder, dispatcher, slot.clone());
    if matches!(kind, SessionKind::Lead) {
        builder = agents::add_lead_tools(builder, worker, slot.clone());
    }
    if families.contains(&McpFamily::Review) {
        builder = review::add_tools(builder, review, slot.clone());
    }
    if families.contains(&McpFamily::Cron) {
        builder = cron::add_tools(builder, cron, slot.clone());
    }
    if families.contains(&McpFamily::Gotify) {
        builder = gotify::add_tools(builder, gotify, slot.clone());
    }
    if families.contains(&McpFamily::Tasks) {
        builder = tasks::add_tools(builder, tasks, slot.clone());
    }
    // Doubly gated: the family is selected AND a client exists - a
    // disabled `[systemone]` leaves no tool a session could try.
    if families.contains(&McpFamily::Systemone)
        && let Some(systemone) = systemone
    {
        builder = systemone::add_tools(builder, systemone);
    }
    if families.contains(&McpFamily::Slack) {
        builder = slack::add_tools(builder, slack, slot);
    }
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
    use crate::mcp::systemone::facade::MockSystemOneFacade;
    use crate::mcp::tasks::facade::MockTasksFacade;
    use crate::mcp::workers::facade::MockWorkerFacade;

    fn fake_key(s: &str) -> SessionSlot {
        SessionSlot::from_str_for_test(s)
    }

    /// The configured shape: a systemone client exists, so its any-caller
    /// tools register for both kinds.
    fn forge_server(kind: SessionKind) -> McpServer {
        forge_server_with(kind, Some(MockSystemOneFacade::new().into_arc()))
    }

    fn forge_server_with(
        kind: SessionKind,
        systemone_facade: Option<Arc<dyn SystemOneFacade>>,
    ) -> McpServer {
        forge_server_families(kind, &McpFamily::all(), systemone_facade)
    }

    fn forge_server_families(
        kind: SessionKind,
        families: &std::collections::BTreeSet<McpFamily>,
        systemone_facade: Option<Arc<dyn SystemOneFacade>>,
    ) -> McpServer {
        build_forge_server(
            ForgeServerFacades {
                workspace: MockWorkspaceFacade::new().into_arc(),
                worker: MockWorkerFacade::new().into_arc(),
                review: MockReviewFacade::new().into_arc(),
                cron: MockCronFacade::new().into_arc(),
                gotify: MockGotifyFacade::new().into_arc(),
                slack: MockSlackFacade::new().into_arc(),
                tasks: MockTasksFacade::new().into_arc(),
                systemone: systemone_facade,
            },
            families,
            fake_key("test"),
            kind,
        )
    }

    /// Every tool name the server registered, read off its debug
    /// listing - which is exactly the set the LLM is offered.
    fn registered_names(kind: SessionKind) -> Vec<String> {
        names_of(&forge_server(kind))
    }

    fn names_of(server: &McpServer) -> Vec<String> {
        let debug = format!("{server:?}");
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

    /// The names the agents family has retired. None may survive: an
    /// alias would leave two names for one verb in the shipped text,
    /// which is the confusion the merge exists to remove.
    ///
    /// Two waves. The eleven `peers__*` / `workers__*` verbs the family
    /// replaced, and the two the family then folded into
    /// `agents__send_message` - a reply is another message, so the split
    /// between them was never a thing a message had to be.
    const OLD_NAMES: [&str; 13] = [
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
        "agents__tell",
        "agents__ask",
    ];

    /// Every group that is any-caller: both session kinds manage their
    /// own project's reviews, crons, tasks, subscriptions and decisions.
    const ANY_CALLER_TOOLS: [&str; 30] = [
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
        "systemone__ask_noul",
        "systemone__ask_choice",
        "systemone__ask_score",
    ];

    #[test]
    fn a_lead_sees_all_seven() {
        assert_eq!(
            agents_tools(SessionKind::Lead),
            [
                "agents__capacity",
                "agents__despawn",
                "agents__list",
                "agents__send_message",
                "agents__spawn",
                "agents__update",
                "agents__whoami",
            ],
        );
    }

    #[test]
    fn a_worker_sees_only_the_shared_three() {
        // The role gate: a worker gains the cross-project reach the
        // merge adds, and must NOT gain a lead-only verb with it.
        assert_eq!(
            agents_tools(SessionKind::Worker),
            ["agents__list", "agents__send_message", "agents__whoami"],
        );
    }

    #[test]
    fn the_retired_names_are_gone() {
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

    /// The injection gate: without a configured `[systemone]` the decision
    /// tools must not appear at all - a session cannot try what forge
    /// chose not to offer.
    #[test]
    fn systemone_tools_absent_without_a_client() {
        for kind in [SessionKind::Lead, SessionKind::Worker] {
            let names = names_of(&forge_server_with(kind, None));
            for absent in ["systemone__ask_noul", "systemone__ask_choice", "systemone__ask_score"] {
                assert!(
                    !names.contains(&absent.to_owned()),
                    "{absent} must be absent without a client for {kind:?}: {names:?}"
                );
            }
        }
    }

    /// Withhold every toggleable family and only the always-on agents
    /// core survives: the four shared verbs for a worker, all eight for
    /// a lead.
    #[test]
    fn withholding_every_family_leaves_only_the_agents_core() {
        let none: std::collections::BTreeSet<McpFamily> = std::collections::BTreeSet::new();
        let mut worker = names_of(&forge_server_families(
            SessionKind::Worker,
            &none,
            Some(MockSystemOneFacade::new().into_arc()),
        ));
        worker.sort();
        assert_eq!(
            worker,
            ["agents__list", "agents__send_message", "agents__whoami"],
            "a withheld family leaves no trace in a worker's tool list"
        );

        let lead = names_of(&forge_server_families(SessionKind::Lead, &none, None));
        assert!(lead.contains(&"agents__spawn".to_owned()), "{lead:?}");
        assert!(
            !lead.iter().any(|name| {
                name.starts_with("cron__")
                    || name.starts_with("systemone__")
                    || name.starts_with("slack__")
            }),
            "no toggleable family registers for a lead with everything withheld: {lead:?}"
        );
    }

    /// A narrow selection registers exactly the core plus the chosen
    /// family.
    #[test]
    fn a_cron_only_selection_registers_the_core_and_cron() {
        let families: std::collections::BTreeSet<McpFamily> =
            [McpFamily::Cron].into_iter().collect();
        let mut names = names_of(&forge_server_families(SessionKind::Worker, &families, None));
        names.sort();
        assert_eq!(
            names,
            [
                "agents__list",
                "agents__send_message",
                "agents__whoami",
                "cron__create",
                "cron__delete",
                "cron__list",
            ],
            "{names:?}"
        );
    }

    /// A stored selection resolves to the canonical set: absent and
    /// empty both mean every family, known names land in wire order, and
    /// a name no longer selectable is dropped rather than widening or
    /// narrowing the surface silently.
    #[test]
    fn stored_family_names_resolve_to_the_canonical_set() {
        assert_eq!(resolve_mcp_families(None), McpFamily::all(), "absent means every family");
        assert_eq!(resolve_mcp_families(Some(&[])), McpFamily::all(), "empty means every family");

        let stored = vec!["slack".to_owned(), "cron".to_owned()];
        assert_eq!(
            resolve_mcp_families(Some(&stored)),
            [McpFamily::Cron, McpFamily::Slack].into_iter().collect(),
            "known names resolve into the set"
        );

        let with_unknown = vec!["bogus".to_owned(), "tasks".to_owned()];
        assert_eq!(
            resolve_mcp_families(Some(&with_unknown)),
            [McpFamily::Tasks].into_iter().collect(),
            "an unknown name is dropped"
        );
    }

    /// The family set and the systemone client are independent gates: a
    /// set naming systemone injects nothing when the section is off, and
    /// a set omitting it injects nothing even when a client exists.
    #[test]
    fn the_family_set_and_the_systemone_client_gate_independently() {
        let without_systemone: std::collections::BTreeSet<McpFamily> =
            McpFamily::all().into_iter().filter(|family| *family != McpFamily::Systemone).collect();
        let names = names_of(&forge_server_families(
            SessionKind::Worker,
            &without_systemone,
            Some(MockSystemOneFacade::new().into_arc()),
        ));
        assert!(
            !names.iter().any(|name| name.starts_with("systemone__")),
            "a client alone does not inject the family: {names:?}"
        );

        let names = names_of(&forge_server_families(SessionKind::Worker, &McpFamily::all(), None));
        assert!(
            !names.iter().any(|name| name.starts_with("systemone__")),
            "[systemone] off injects nothing however the set reads: {names:?}"
        );
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

    /// The span of the `OLD_NAMES` declaration itself, which is the only
    /// place a bare quoted retired name is allowed to sit.
    ///
    /// **Scoped to the declaration rather than to a line's shape.** The list
    /// has to name the retired tools in order to assert them away, and a line
    /// that merely LOOKS like an entry is not the list: a retired name
    /// re-registered in a tool list has exactly that shape, and it is the
    /// alias this scan exists to refuse. `None` when the span will
    /// not read, which fails the scan loudly rather than exempting every
    /// entry.
    fn old_names_declaration(text: &str) -> Option<std::ops::RangeInclusive<usize>> {
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.iter().position(|line| line.contains("const OLD_NAMES"))?;
        let end = start + lines[start..].iter().position(|line| line.trim() == "];")?;
        Some(start..=end)
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

    /// The tracked source files, read from the working tree, paired with how
    /// many were listed.
    ///
    /// Tracked content, not a filesystem walk. A surface forge ships is a
    /// file in the repository, and a walk descends into everything git
    /// excludes - `docs/superpowers/`, `.claude/plans/`, another
    /// worktree's checkout - so it reports a clean tree or a red one
    /// depending on who else is using the machine. The recorded baselines
    /// are `.jsonl`, so the extension filter leaves them out: a capture
    /// holds whatever the capture machine printed.
    ///
    /// **The client's own files count, and they are the ones this most
    /// rewrote.** A rename that reached `client/src/` and stopped there leaves
    /// the page calling a tool nothing registers, which is the failure the scan
    /// exists to catch - so `.ts` and `.svelte` are in, and the exemption is
    /// line-based like everything else.
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
                    Some("rs" | "md" | "ts" | "svelte")
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
            "read {} of {listed} tracked source files, so the scan is not the tree it claims",
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
            let declaration = old_names_declaration(&text);
            for (number, line) in lines.iter().enumerate() {
                let exempt = exempted_names(&lines, number);
                let unexempted: Vec<&str> = OLD_NAMES
                    .iter()
                    .copied()
                    .filter(|old| line.contains(old) && !exempt.contains(old))
                    .collect();
                let declared = declaration.as_ref().is_some_and(|span| span.contains(&number));
                if !unexempted.is_empty() && !declared {
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

    /// **The entry exemption is scoped to the declaration, not to a line's
    /// shape.** A retired name registered as an alias has exactly the shape of
    /// an entry - a bare quoted string on a line of its own - so exempting by
    /// shape is how a re-created alias would pass this scan while being the
    /// thing the scan exists to refuse. The fixture is built from the list
    /// rather than from a literal, which is what keeps this file's own text
    /// from tripping the scan it tests.
    #[test]
    fn a_bare_quoted_name_is_exempt_only_inside_the_declaration() {
        let listing = format!("let tools = [\n    \"{}\",\n];\n", OLD_NAMES[0]);
        assert!(
            old_names_declaration(&listing).is_none(),
            "a text that declares no list exempts nothing at all",
        );

        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp.rs"),
        )
        .expect("this file reads");
        let span = old_names_declaration(&source).expect("this file declares OLD_NAMES");
        let lines: Vec<&str> = source.lines().collect();
        assert_eq!(
            lines[span.start() + 1].trim().trim_end_matches(',').trim_matches('"'),
            OLD_NAMES[0],
            "the span opens on the first entry",
        );
        assert!(
            !span.contains(&(span.end() + 1)),
            "and a line past the declaration's close is not exempt",
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
