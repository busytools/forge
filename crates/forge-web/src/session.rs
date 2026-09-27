//! The session page: the projects rail, the conversation, and the
//! inspector.
//!
//! Three columns over one sheet. The rail and the inspector collapse to
//! nothing and their handles live in the chat header, so a collapsed pane
//! leaves no edge behind and its control stays reachable. Every column
//! reads the core through the view surface: this module holds no state of
//! its own.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::SystemTime;

use forge_primitives::PermissionMode;
use forge_primitives::SessionLifecycleState;
use forge_primitives::SessionSlot;
use forge_primitives::account::AccountAuth;
use forge_primitives::git::{GitBranch, GitIssueRef};
use forge_primitives::git_diff::{GitDiffFile, GitDiffSnapshot, GitDiffStats, LayerState};
use forge_primitives::slack::{SlackSubscriptionTarget, SlackWatchMode};
use forge_primitives::tasks::{Task, TaskStatus};
use forge_primitives::{
    CronEntry, CronKind, McpServerConnectionStatus, MonitorRecord, MonitorStatus,
};
use forge_sessions::surface::connectors::{GotifyView, SlackView};
use forge_sessions::surface::inspector::{
    McpServers, ProcessEntry, ProcessSnapshot, SessionHeader,
};
use forge_sessions::surface::{AccountsView, Agents, LoadingState, PendingKind, Roster};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::home::{Home, Row, Seed, State};
use crate::icons;
use crate::server::WebState;
use crate::stream::Live;
use crate::work::WorkState;

/// The handle that brings the projects rail back, and the one that brings
/// the inspector back. Both live with the title so they stay clickable
/// while their pane is gone.
const RAIL_TOGGLE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M3 6h18M3 12h18M3 18h18"/></svg>"#;
const INSPECTOR_TOGGLE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M15 4v16"/></svg>"#;

/// What the route found for a slot.
pub enum Found {
    /// The seat is in the roster, so it has a page: one page draws both a
    /// session that is up and a seat nothing is running behind, and only the
    /// chat column tells them apart.
    Page(Markup),
    /// No seat by that name.
    Absent,
}

/// The page's own address for a slot, which is what the home's rows and
/// the rail's rows point at.
pub(crate) fn href(slot: &SessionSlot) -> String {
    format!(
        "/session/{}/{}/{}",
        segment(slot.org()),
        segment(slot.project()),
        segment(slot.label())
    )
}

/// One path segment, escaped. A project's name may hold a slash - the
/// mock's own `companies/steward` does - and an unescaped one would
/// address four segments rather than three.
fn segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            other => {
                out.push('%');
                out.push(char::from(HEX[usize::from(other >> 4)]));
                out.push(char::from(HEX[usize::from(other & 0x0F)]));
            }
        }
    }
    out
}

/// Resolve the slot the route names, then render what it serves: the page,
/// the waking page, or nothing for a slot the roster does not hold.
pub async fn page(
    state: &WebState,
    bound: SocketAddr,
    org: &str,
    project: &str,
    label: &str,
) -> Found {
    let surface = &state.surface;
    let roster = surface.roster();
    let agents = surface.agents();
    let Some(seat) = roster.projects.iter().find(|seat| seat.org == org && seat.name == project)
    else {
        return Found::Absent;
    };
    // A project declares one seat of its own that exists whether or not it
    // has ever run; a worker is a seat only while the roster can name it.
    let slot = if label == "lead" {
        SessionSlot::lead(org, project)
    } else {
        let named = agents.for_project(&seat.key).iter().any(|row| row.label == label)
            || surface.workers().for_project(&seat.key).iter().any(|row| row.label == label);
        if !named {
            return Found::Absent;
        }
        SessionSlot::worker(org, project, label)
    };
    // One walk of the core per page: the roster and the agents the route
    // already holds are the two the page draws from.
    Found::Page(shell(&context(state, bound), &slot, &roster, &agents).await)
}

/// The pieces both pages read the core through, which are the home's own:
/// one view context per page, over the same surface, cache and live state.
fn context(state: &WebState, bound: SocketAddr) -> Home<'_> {
    Home {
        surface: &state.surface,
        work: &state.work,
        live: &state.live,
        bound,
        mark: state.config.mark.as_deref(),
        theme: state.config.theme.as_deref(),
        font: state.config.font.as_deref(),
    }
}

/// The page. One page for both outcomes: the columns are as real for a
/// seat nothing is running behind as for one that is up, and only the chat
/// column says which of the two it is looking at.
async fn shell(home: &Home<'_>, slot: &SessionSlot, roster: &Roster, agents: &Agents) -> Markup {
    let live = Live::lock(home.live).snapshot();
    let accounts = home.surface.accounts();
    let row = agents.all().iter().find(|row| &row.slot == slot);
    let state =
        row.map_or(State::NeverStarted, |row| crate::home::state_of_agent(row, &live.unseen));
    // A lead's row is its project, the way the home names it; a worker's is
    // its own label.
    let name = if slot.label() == "lead" { slot.project() } else { slot.label() };
    // The account a spawn here would bind to, which is the roster's own
    // answer rather than a second walk of the pool.
    let chip = roster
        .projects
        .iter()
        .find(|seat| seat.org == slot.org() && seat.name == slot.project())
        .and_then(|seat| roster.chip_for(&seat.key))
        .map(|chip| chip.account_name);
    let waking = !roster.has_agent(slot);
    let header = home.surface.header(slot);

    html! {
        (DOCTYPE)
        html lang="en" {
            (crate::server::page_head(
                &format!("forge \u{b7} {} \u{b7} {name}", slot.project()),
                home.theme,
                home.font,
            ))
            // The checkboxes are the pane state: CSS-only, so a collapsed
            // pane needs no script and a reload does not forget it while
            // the page is open. Checked is COLLAPSED.
            body {
                (icons::sprite())
                input type="checkbox" id="l" hidden;
                input type="checkbox" id="r" hidden;
                div .app {
                    aside .rail .left {
                        div .banner {
                            span .t { "projects" }
                            span .n .ml { (fleet_count(roster)) }
                        }
                        div .scroll { (rail(home, roster, agents, slot).await) }
                    }
                    main .chat {
                        div .sess {
                            label .pane-tog .tog-l for="l" title="projects" {
                                (PreEscaped(RAIL_TOGGLE))
                            }
                            label .pane-tog .tog-r for="r" title="inspector" {
                                (PreEscaped(INSPECTOR_TOGGLE))
                            }
                            span .dot .(mark_of(state)) {}
                            span .nm { (name) }
                            span .mono .dim { (slot.org()) }
                            span .facts {
                                (header_facts(&header))
                                (account_chip(&accounts, chip.as_deref()))
                            }
                        }
                        div .conv {
                            (chat_body(waking, row.and_then(|row| row.reason.as_deref())))
                        }
                    }
                    aside .rail .right {
                        div .banner {
                            span .t { "inspector" }
                            span .n .ml { (slot.project()) }
                        }
                        div .scroll { (inspector(home, roster, slot).await) }
                    }
                }
            }
        }
    }
}

/// The header's four facts: the model the session resolved, the effort it
/// runs at, the mode a hook observed and how full its context is. An
/// unstated one draws a dash rather than dropping the fact, so the header
/// keeps its shape from the moment a seat opens.
fn header_facts(header: &SessionHeader) -> Markup {
    let model = header.model.as_ref().map_or_else(
        || "\u{2014}".to_owned(),
        |model| {
            if model.display_name_long.is_empty() {
                model.resolved_id.clone()
            } else {
                model.display_name_long.clone()
            }
        },
    );
    let mode = header
        .permission_mode
        .map(|mode| (mode.as_wire(), format!("perm {}", perm_class(mode)).trim_end().to_owned()));
    let percent = header.context.percent;
    html! {
        span { span .fk { "model" } " " span .v { (model) } }
        span .sep { "\u{b7}" }
        span { span .fk { "effort" } " " span .v { (header.effort.as_stored()) } }
        span .sep { "\u{b7}" }
        span {
            span .fk { "mode" } " "
            @match mode {
                Some((wire, class)) => span .(class) { (wire) },
                None => span .perm { "\u{2014}" },
            }
        }
        span .sep { "\u{b7}" }
        span .cm {
            span .fk { "ctx" }
            span .tk { span .fl style=(format!("width:{}%", percent.unwrap_or(0))) {} }
            span .v {
                @match percent {
                    Some(percent) => (format!("{percent}%")),
                    None => "\u{2014}",
                }
            }
        }
    }
}

/// The class a permission mode's chip carries, so the colour says how much
/// the session is allowed to do without being asked.
fn perm_class(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Auto | PermissionMode::AcceptEdits => "auto",
        PermissionMode::Plan => "plan",
        PermissionMode::BypassPermissions => "bypass",
        PermissionMode::Ask | PermissionMode::DontAsk => "",
    }
}

/// The projects rail: every declared project, grouped by the strongest
/// state among its own rows, with its workers under it.
async fn rail(home: &Home<'_>, roster: &Roster, agents: &Agents, slot: &SessionSlot) -> Markup {
    let unseen = Live::lock(home.live).snapshot().unseen;
    let mut groups: [Vec<Pane>; 3] = [Vec::new(), Vec::new(), Vec::new()];

    for project in &roster.projects {
        let rows = agents.for_project(&project.key);
        let lead_slot = SessionSlot::lead(&project.org, &project.name);
        // A project nobody has started is a row of its own rather than
        // nothing at all: the home draws the same one.
        let lead_seed = if let Some(agent) = rows.first() {
            let mut seed = Seed::from_agent(agent, None, &unseen);
            seed.name.clone_from(&project.name);
            seed
        } else {
            let last_ran = project.sessions.iter().filter_map(|view| view.last_activity).max();
            crate::home::dormant_seed(&lead_slot, project.name.clone(), last_ran)
        };
        let lead = crate::home::row_for(home, roster, lead_seed).await;
        let mut workers = Vec::new();
        for agent in rows.iter().skip(1) {
            workers.push(
                crate::home::row_for(home, roster, Seed::from_agent(agent, None, &unseen)).await,
            );
        }

        let group = [&lead]
            .into_iter()
            .chain(workers.iter())
            .map(Group::of)
            .min_by_key(|group| group.rank())
            .unwrap_or(Group::Asleep);
        let why = why_of(std::iter::once(&lead).chain(workers.iter()));
        let age = crate::home::when_of(lead.state, lead.last_activity);
        groups[group.rank() as usize].push(Pane {
            name: project.name.clone(),
            org: project.org.clone(),
            current: project.org == slot.org() && project.name == slot.project(),
            asleep: group == Group::Asleep,
            age,
            lead,
            workers,
            why,
        });
    }

    let [needs, working, asleep] = groups;
    html! {
        @for (heading, panes, class) in [
            ("needs you", &needs, "state needs"),
            ("working", &working, "state"),
            ("asleep", &asleep, "state"),
        ] {
            @if !panes.is_empty() {
                div class=(class) { (heading) }
                @for pane in panes {
                    (pane_markup(pane))
                }
            }
        }
    }
}

/// Where a project sits in the rail. The three groups are also the order
/// they read in, so the rank is the order and the array index both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Needs,
    Working,
    Asleep,
}

impl Group {
    /// The group a row belongs in. An ask outranks the lifecycle: a session
    /// holds a prompt while the core still calls it idle, and what it is
    /// waiting on is a person.
    fn of(row: &Row) -> Self {
        if row.pending.is_some() {
            return Self::Needs;
        }
        match row.state {
            State::Lifecycle(
                SessionLifecycleState::Attention
                | SessionLifecycleState::Failed
                | SessionLifecycleState::AuthRequired,
            ) => Self::Needs,
            State::NeverStarted
            | State::Lifecycle(
                SessionLifecycleState::Sleeping | SessionLifecycleState::LoggedOut,
            ) => Self::Asleep,
            State::Lifecycle(
                SessionLifecycleState::Running
                | SessionLifecycleState::Spawning
                | SessionLifecycleState::Idle,
            )
            | State::Unseen => Self::Working,
        }
    }

    fn rank(self) -> usize {
        match self {
            Self::Needs => 0,
            Self::Working => 1,
            Self::Asleep => 2,
        }
    }
}

/// One project as the rail draws it.
struct Pane {
    name: String,
    org: String,
    /// The project the page is showing.
    current: bool,
    asleep: bool,
    age: String,
    lead: Row,
    workers: Vec<Row>,
    /// What the project is waiting on, when it is waiting on anything.
    why: Option<Why>,
}

/// The reason line: what a person has to do about this row, in the row's
/// own words rather than a second vocabulary for the same two asks.
struct Why {
    line: String,
    bad: bool,
}

/// The line under a project: its first ask, else its first failure. A
/// project whose worker is held reads as held, whether or not its lead is
/// the one held.
fn why_of<'a>(rows: impl Iterator<Item = &'a Row>) -> Option<Why> {
    let rows: Vec<&Row> = rows.collect();
    for row in &rows {
        if let Some(pending) = row.pending {
            return Some(Why { line: waiting_on(pending), bad: false });
        }
    }
    for row in &rows {
        if matches!(
            row.state,
            State::Lifecycle(SessionLifecycleState::Failed | SessionLifecycleState::AuthRequired)
        ) {
            return Some(Why {
                line: row.reason.clone().unwrap_or_else(|| "not running".to_owned()),
                bad: true,
            });
        }
    }
    None
}

fn pane_markup(pane: &Pane) -> Markup {
    html! {
        div class=(if pane.current { "pj cur" } else { "pj" }) {
            div .pr {
                span .dot .(mark_of(pane.lead.state)) {}
                span .nm { a href=(href(&pane.lead.slot)) { (&pane.name) } }
                span .org { (&pane.org) }
                @if pane.asleep {
                    span .age { (&pane.age) }
                } @else {
                    (close_chip())
                }
            }
            @if let Some(why) = &pane.why {
                div class=(if why.bad { "why bad" } else { "why" }) { (&why.line) }
            }
            @for worker in &pane.workers {
                div .wk {
                    span .dot .(mark_of(worker.state)) {}
                    span .nm { a href=(href(&worker.slot)) { (worker.slot.label()) } }
                    (close_chip())
                }
            }
        }
    }
}

/// The close chip every row carries. Nothing in this view can close a
/// session yet - that is the dispatch path, which lands separately - so the
/// chip is drawn unavailable rather than as a control that does nothing when
/// it is clicked.
fn close_chip() -> Markup {
    html! {
        span .x aria-disabled="true" title="closing a session is not available yet" {
            "\u{2715}"
        }
    }
}

/// What a held session is waiting on a person for.
fn waiting_on(pending: PendingKind) -> String {
    match pending {
        PendingKind::Question => "asked you a question",
        PendingKind::Permission => "a permission prompt is waiting",
    }
    .to_owned()
}

/// The inspector: one section per subject, each collapsed to a name and a
/// summary and opening in place. A section is drawn when there is
/// something behind it - a project with no tasks has no tasks section -
/// because a section that is always there says nothing when it is empty.
async fn inspector(home: &Home<'_>, roster: &Roster, slot: &SessionSlot) -> Markup {
    let (work, diff) = match roster.cwd_for(slot) {
        Some(cwd) => {
            (Some(home.work.snapshot(slot, &cwd).await), Some(home.work.diff(slot, &cwd).await))
        }
        None => (None, None),
    };
    let tasks = roster.tasks_for_project(slot.project());
    let crons = roster.crons_for_project(slot.project());
    let connectors = home.surface.connectors(Some(slot.project()));
    let attributed = home.surface.subagent_attribution(slot);
    let servers = home.surface.mcp_servers(slot);
    let walk = home.surface.processes(slot);
    let monitors = home.surface.monitors(slot);

    html! {
        @if let Some(work) = &work {
            (git_section(work, diff.as_ref()))
        }
        @if !tasks.is_empty() {
            (tasks_section(&tasks))
        }
        @if !attributed.is_empty() {
            (subagents_section(&attributed))
        }
        @if !crons.is_empty() {
            (schedules_section(&crons))
        }
        @if connectors.gotify.connected || !connectors.gotify.subscriptions.is_empty() {
            (gotify_section(&connectors.gotify))
        }
        @if !connectors.slack.connected_workspaces.is_empty()
            || !connectors.slack.subscriptions.is_empty()
        {
            (slack_section(&connectors.slack))
        }
        @if let Some(servers) = &servers {
            @if !servers.servers.is_empty() || servers.error.is_some() {
                (mcp_section(servers))
            }
        }

        @if let Some(walk) = walk.as_ref().filter(|walk| !walk.processes.is_empty()) {
            (processes_section(walk))
        }
        @if !monitors.is_empty() {
            (monitors_section(&monitors))
        }
    }
}

/// The subagents section: which agent type ran which of this session's
/// tool calls.
///
/// This is the session's own attribution, not the CLI's catalogue of the
/// agent types that exist: a session that has had no sub-agent run has
/// nothing here, whether or not it could offer one.
fn subagents_section(attributed: &HashMap<String, String>) -> Markup {
    let mut per_type: Vec<(&str, usize)> = Vec::new();
    for agent_type in attributed.values() {
        match per_type.iter_mut().find(|(name, _)| name == agent_type) {
            Some((_, calls)) => *calls += 1,
            None => per_type.push((agent_type.as_str(), 1)),
        }
    }
    per_type.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    let types = per_type.len();
    let body = html! {
        @for (agent_type, calls) in &per_type {
            div .kv {
                span .k { (agent_type) }
                span .v { (call_count(*calls)) }
            }
        }
    };
    section(false, "subagents", "subagents", &types.to_string(), &body)
}

/// How much work one agent type ran, counted so that one reads as one.
fn call_count(calls: usize) -> String {
    if calls == 1 { "1 call".to_owned() } else { format!("{calls} calls") }
}

/// The MCP servers this session's bridge reported, with the reason a read
/// that failed came back empty rather than reading as nothing configured.
fn mcp_section(servers: &McpServers) -> Markup {
    let summary = if servers.servers.is_empty() {
        "failed".to_owned()
    } else {
        servers.servers.len().to_string()
    };
    let body = html! {
        @for server in &servers.servers {
            div .kv {
                span .k { (server.name) }
                span .v { (connection_label(server.status)) }
            }
        }
        @if let Some(error) = &servers.error {
            div .note { (error) }
        }
    };
    section(false, "mcp", "mcp servers", &summary, &body)
}

/// What a server's connection state reads as, in the page's words rather
/// than the wire's.
fn connection_label(status: McpServerConnectionStatus) -> &'static str {
    match status {
        McpServerConnectionStatus::Connected => "connected",
        McpServerConnectionStatus::Failed => "failed",
        McpServerConnectionStatus::NeedsAuth => "needs sign-in",
        McpServerConnectionStatus::Pending => "connecting",
        McpServerConnectionStatus::Disabled => "disabled",
    }
}

/// The processes section: claude's descendant tree, a row each, with the
/// tree's shape carried by the row's indent.
fn processes_section(walk: &ProcessSnapshot) -> Markup {
    let count = walk.processes.len();
    let tree = process_tree(walk);
    let body = html! {
        @for (entry, depth) in tree {
            div .kv {
                span .k {
                    @for _ in 0..depth { (PreEscaped("&nbsp;&nbsp;")) }
                    (entry.name)
                }
                span .v { (format!("{} \u{b7} {}", memory_label(entry.memory_bytes), entry.pid)) }
            }
        }
    };
    section(false, "processes", "processes", &count.to_string(), &body)
}

/// The walk as a tree: every row with how deep it sits, parents before
/// their children, and the walk's own order within a sibling group.
///
/// The scan returns entries by memory rather than by parentage, so drawing
/// them in that order would indent a row under whatever happened to come
/// before it. A row whose parent the walk did not carry is a root of its
/// own, which is what makes a partial snapshot still list everything in
/// it.
fn process_tree(walk: &ProcessSnapshot) -> Vec<(&ProcessEntry, usize)> {
    let mut children_of: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    let mut present: HashSet<u32> = HashSet::new();
    for entry in &walk.processes {
        children_of.entry(entry.parent_pid).or_default().push(entry);
        present.insert(entry.pid);
    }

    let mut placed: HashSet<u32> = HashSet::new();
    let mut tree = Vec::with_capacity(walk.processes.len());
    for entry in &walk.processes {
        if !present.contains(&entry.parent_pid) && placed.insert(entry.pid) {
            walk_subtree(entry, 0, &children_of, &mut placed, &mut tree);
        }
    }
    // A pid cycle reaches no root, and neither does a subtree hanging off
    // one. Every row is still drawn, once.
    for entry in &walk.processes {
        if placed.insert(entry.pid) {
            walk_subtree(entry, 0, &children_of, &mut placed, &mut tree);
        }
    }
    tree
}

/// One row and its descendants. `placed` is what stops a cycle: a pid the
/// walk already drew is not drawn again.
fn walk_subtree<'a>(
    entry: &'a ProcessEntry,
    depth: usize,
    children_of: &HashMap<u32, Vec<&'a ProcessEntry>>,
    placed: &mut HashSet<u32>,
    tree: &mut Vec<(&'a ProcessEntry, usize)>,
) {
    tree.push((entry, depth));
    for child in children_of.get(&entry.pid).into_iter().flatten() {
        if placed.insert(child.pid) {
            walk_subtree(child, depth + 1, children_of, placed, tree);
        }
    }
}

/// Resident memory, in the unit the reader thinks in. Integer arithmetic
/// all the way down, and the same units the TUI's own row uses, so the two
/// views read the same tree the same way.
fn memory_label(bytes: u64) -> String {
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
        format!("{}.{} GB", bytes / GB, (bytes % GB) / (GB / 10))
    }
}

/// The monitors section. Monitors live here and not in the chat, so this
/// is the only surface that says what a session is watching.
fn monitors_section(monitors: &[MonitorRecord]) -> Markup {
    let running = monitors.iter().filter(|monitor| !monitor.status.is_terminal()).count();
    let summary = format!("{running} running");
    let body = html! {
        @for monitor in monitors {
            div .sa {
                div .sh {
                    @if monitor.status.is_terminal() {
                        span .st { (icons::icon("check", "")) }
                    } @else {
                        span .st { span .ring style="width:8px;height:8px" {} }
                    }
                    (icons::icon("monitors", "gl"))
                    span .nm { (&monitor.description) }
                    span .n { (monitor_label(monitor)) }
                }
                @if monitor.status.is_terminal() {
                    div .settled { "settled - its output stays in the transcript" }
                } @else {
                    div .tt { span .tg { "$" } " " (&monitor.command) }
                }
            }
        }
    };
    section(false, "monitors", "monitors", &summary, &body)
}

/// The trailing word on a monitor's own row: how it ended, or what it is
/// while it runs.
fn monitor_label(monitor: &MonitorRecord) -> String {
    match monitor.status {
        MonitorStatus::Running if monitor.persistent => "persistent".to_owned(),
        MonitorStatus::Running => "running".to_owned(),
        MonitorStatus::Completed => "completed".to_owned(),
        MonitorStatus::Stopped => "stopped".to_owned(),
        MonitorStatus::TimedOut => "timed out".to_owned(),
    }
}

/// One section: an icon, a name, a summary of what is behind it, and the
/// body it opens on. The arrow is one chevron turned by the open state, so a
/// closed section and an open one draw the same sprite.
fn section(open: bool, icon_name: &str, name: &str, summary: &str, body: &Markup) -> Markup {
    html! {
        details .sec open[open] {
            summary {
                (icons::icon(icon_name, "gl"))
                (name)
                @if !summary.is_empty() {
                    span .c2 { (summary) }
                }
                (icons::chevron(""))
            }
            div .sb { (body) }
        }
    }
}

/// The git section: the branch the session's tree is on and what moved in
/// it, the pull request that tree belongs to, and the files themselves.
fn git_section(work: &WorkState, diff: Option<&GitDiffSnapshot>) -> Markup {
    let (open, summary, body) = match diff {
        Some(diff) => (git_has_body(work, diff), git_summary(diff), git_body(work, diff)),
        None => (false, String::new(), Markup::default()),
    };
    section(open, "git", "git", &summary, &body)
}

/// Whether the section has anything to open on. The branch it names is not
/// enough: a clean tree with no pull request would lead the inspector with
/// an open section and nothing under it.
fn git_has_body(work: &WorkState, diff: &GitDiffSnapshot) -> bool {
    diff.pr.is_some()
        || matches!(diff.worktree, LayerState::Populated(_))
        || layer_note(&diff.worktree).is_some()
        || crate::home::gate_line(work.gate).is_some()
}

/// The section's line: the branch, and how many files the body below it
/// lists. Both come from the one scan, so the count cannot describe a
/// different read than the list does. The working tree's own count is a
/// different read again - it includes untracked files, which a diff
/// cannot show - and deriving this line from it would put a number over a
/// list that does not match it.
fn git_summary(diff: &GitDiffSnapshot) -> String {
    let branch = match &diff.branch {
        GitBranch::Named(name) => Some(name.as_str()),
        GitBranch::Detached => Some("detached"),
        GitBranch::NoRepo | GitBranch::Unknown => None,
    };
    let files = match &diff.worktree {
        LayerState::Populated(stats) => Some(stats.total_files),
        LayerState::Clean | LayerState::ScanFailed => None,
    };
    crate::home::branch_and_files(branch, files)
}

/// What has moved, the PR it belongs to, and the files by directory. The
/// per-file status the mock draws is not here: the scan reports numstat,
/// not `M`/`A`.
fn git_body(work: &WorkState, diff: &GitDiffSnapshot) -> Markup {
    let stats = match &diff.worktree {
        LayerState::Populated(stats) => Some(stats),
        LayerState::Clean | LayerState::ScanFailed => None,
    };
    let files = stats.map_or(&[][..], |stats| stats.files.as_slice());
    html! {
        @if let Some(pr) = &diff.pr {
            div .kv {
                span .k { "PR #" (pr.number) }
                @if !diff.closes.is_empty() {
                    span .v .a { "\u{2192} closes " (closes_of(&diff.closes)) }
                }
            }
        }
        @if let Some(stats) = stats {
            div .kv {
                span .k { "uncommitted" }
                span .v {
                    span .pm { "+" (stats.total_added) }
                    " "
                    span .mm { "\u{2212}" (stats.total_removed) }
                }
            }
        }
        @if let Some(note) = layer_note(&diff.worktree) {
            div .kv { span .k { (note) } }
        } @else if files.is_empty() {
            @if let Some(line) = crate::home::gate_line(work.gate) {
                div .kv { span .k { (line) } }
            }
        } @else {
            @for (dir, files) in by_dir(files) {
                @if !dir.is_empty() {
                    div .dir { (dir) "/" }
                }
                @for file in files {
                    div .file {
                        span .p { (file_name(&file.path)) }
                        span .pm { "+" (file.added) }
                        span .mm { "\u{2212}" (file.removed) }
                    }
                }
            }
            @if let Some(stats) = stats.filter(|stats| files.len() < stats.total_files) {
                div .dir { "\u{2026}and " (stats.total_files - files.len()) " more" }
            }
        }
    }
}

/// What the section says about a layer that is not listing files. A scan
/// that failed is the one state that would otherwise read as a clean tree:
/// the branch is there, no file is listed, and nothing says why.
fn layer_note(layer: &LayerState<GitDiffStats>) -> Option<&'static str> {
    match layer {
        LayerState::ScanFailed => Some("its changes could not be read"),
        LayerState::Clean | LayerState::Populated(_) => None,
    }
}

/// The issues an open PR closes, in the order the scan returned them.
fn closes_of(issues: &[GitIssueRef]) -> String {
    issues.iter().map(|issue| format!("#{}", issue.number)).collect::<Vec<_>>().join(" ")
}

/// The files by the directory they sit in, in the order the scan returned
/// them. The scan orders by how much each file changed, so one directory's
/// files need not arrive together: the heading is drawn where the
/// directory first appears, and its later files join it there.
fn by_dir(files: &[GitDiffFile]) -> Vec<(&str, Vec<&GitDiffFile>)> {
    let mut groups: Vec<(&str, Vec<&GitDiffFile>)> = Vec::new();
    for file in files {
        let dir = file.path.rsplit_once('/').map_or("", |(dir, _)| dir);
        match groups.iter_mut().find(|(name, _)| *name == dir) {
            Some((_, group)) => group.push(file),
            None => groups.push((dir, vec![file])),
        }
    }
    groups
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The tasks section: what the project holds, in the order a person reads
/// them rather than the order the store returns them.
fn tasks_section(tasks: &[Task]) -> Markup {
    let done = tasks.iter().filter(|task| task.status == TaskStatus::Completed).count();
    let mut ordered: Vec<&Task> = tasks.iter().collect();
    ordered.sort_by_key(|task| crate::home::status_rank(task.status));
    let body = html! {
        @for task in ordered {
            div class=(task_class(task.status)) {
                span .b {}
                span {
                    div .s { (&task.subject) }
                    div .meta {
                        @if let Some(owner) = &task.owner {
                            b { (owner.label()) } " \u{b7} "
                        }
                        (task_meta(task))
                    }
                }
            }
        }
    };
    section(false, "tasks", "tasks", &format!("{done} of {}", tasks.len()), &body)
}

fn task_class(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::InProgress => "tk now",
        TaskStatus::Completed => "tk done",
        TaskStatus::Blocked => "tk blocked",
        TaskStatus::Pending => "tk",
    }
}

/// A task's facts besides its owner: how far along it is, what it
/// produced, and how long it was thought to take.
fn task_meta(task: &Task) -> String {
    let mut parts = vec![crate::home::chip_for(task.status).to_owned()];
    if let Some(artifact) = &task.artifact {
        parts.push(crate::home::artifact_label(artifact));
    }
    if let Some(estimate) = &task.estimate {
        parts.push(estimate.clone());
    }
    parts.join(" \u{b7} ")
}

/// The schedules section: the durable crons that fire into this project,
/// with when each one is next due.
fn schedules_section(crons: &[CronEntry]) -> Markup {
    let body = html! {
        @for cron in crons {
            div .kv {
                span .k { (cron_label(cron)) }
                span .v { (until_of(cron.next_fire)) " \u{b7} " (kind_of(&cron.kind)) }
            }
        }
    };
    section(false, "schedules", "schedules", &crons.len().to_string(), &body)
}

/// What a schedule is called: its own description, else the first line of
/// the prompt it fires.
fn cron_label(cron: &CronEntry) -> String {
    cron.description
        .clone()
        .or_else(|| cron.prompt.lines().next().map(str::to_owned))
        .unwrap_or_default()
}

fn kind_of(kind: &CronKind) -> &'static str {
    match kind {
        CronKind::Recurring(_) => "recurring",
        CronKind::Once(_) => "one-shot",
    }
}

/// How long until `at`. A time the clock has already passed is due rather
/// than a countdown into the past.
fn until_of(at: SystemTime) -> String {
    let Ok(remaining) = at.duration_since(SystemTime::now()) else {
        return "due now".to_owned();
    };
    match remaining.as_secs() {
        0..=59 => "in a minute".to_owned(),
        seconds if seconds < 3600 => format!("in {}m", seconds / 60),
        seconds if seconds < 86_400 => format!("in {}h", seconds / 3600),
        seconds => format!("in {}d", seconds / 86_400),
    }
}

/// The gotify section: the stream's liveness and what this project is
/// subscribed to on it.
///
/// One row per subject rather than one per subscription: the mockup labels
/// them `apps` and `priority`, and a delivery is let through when any
/// subscription matches, so the two facts are each read across the set.
fn gotify_section(view: &GotifyView) -> Markup {
    let summary = if view.connected { "connected" } else { "not connected" };
    let mut apps: Vec<&str> = Vec::new();
    for app in view.subscriptions.iter().flat_map(|sub| &sub.applications) {
        if !apps.contains(&app.as_str()) {
            apps.push(app);
        }
    }
    let floor = if view.subscriptions.iter().any(|sub| sub.min_priority.is_none()) {
        None
    } else {
        view.subscriptions.iter().filter_map(|sub| sub.min_priority).min()
    };
    let body = html! {
        div .kv { span .k { "apps" } span .v { (apps.join(", ")) } }
        div .kv {
            span .k { "priority" }
            span .v { (floor.map_or_else(|| "any".to_owned(), |floor| format!(">={floor}"))) }
        }
    };
    section(false, "gotify", "gotify", summary, &body)
}

/// The slack section: one row per workspace, and one per subscription
/// under it.
fn slack_section(view: &SlackView) -> Markup {
    let mut workspaces: Vec<&str> = view.connected_workspaces.keys().map(String::as_str).collect();
    for sub in &view.subscriptions {
        if !workspaces.contains(&sub.workspace.as_str()) {
            workspaces.push(&sub.workspace);
        }
    }
    let summary =
        format!("{} workspace{}", workspaces.len(), if workspaces.len() == 1 { "" } else { "s" });
    let body = html! {
        @for workspace in &workspaces {
            div .kv {
                span .k { (workspace) }
                span .v {
                    @if view.connected_workspaces.get(*workspace).copied().unwrap_or(false) {
                        "connected"
                    } @else {
                        "not connected"
                    }
                }
            }
            @for sub in view.subscriptions.iter().filter(|sub| sub.workspace == *workspace) {
                div .kv {
                    span .k { "\u{a0}\u{a0}" (target_of(&sub.target)) }
                    span .v { (mode_of(&sub.target)) }
                }
            }
        }
    };
    section(false, "slack", "slack", &summary, &body)
}

/// What a Slack subscription watches: a conversation by its name, or the
/// class it covers.
fn target_of(target: &SlackSubscriptionTarget) -> String {
    match target {
        SlackSubscriptionTarget::DirectMessages => "direct messages".to_owned(),
        SlackSubscriptionTarget::Mentions => "mentions anywhere".to_owned(),
        SlackSubscriptionTarget::Conversation { id, name, .. } => {
            name.clone().unwrap_or_else(|| id.clone())
        }
    }
}

/// What a Slack subscription lets through.
fn mode_of(target: &SlackSubscriptionTarget) -> &'static str {
    match target {
        SlackSubscriptionTarget::DirectMessages => "every message",
        SlackSubscriptionTarget::Mentions => "mentions only",
        SlackSubscriptionTarget::Conversation { mode, .. } => match mode {
            SlackWatchMode::All => "every message",
            SlackWatchMode::MentionsOnly => "mentions only",
        },
    }
}

/// What the chat column says while the conversation itself is not there
/// yet: the seat's own state, and why when the core recorded a reason.
///
/// It claims nothing about a spawn. This page cannot start a session, so a
/// line saying one is coming would be a promise no code keeps, and a seat
/// whose spawn failed would carry a failure mark above a line saying it is
/// connecting.
fn chat_body(waking: bool, reason: Option<&str>) -> Markup {
    if !waking {
        return Markup::default();
    }
    html! {
        div .hold .off {
            "not running"
            span .sub { (reason.unwrap_or("this seat has no session behind it")) }
        }
    }
}

/// How many projects have a session behind them, out of how many are
/// declared.
fn fleet_count(roster: &Roster) -> String {
    let live = roster
        .projects
        .iter()
        .filter(|seat| roster.has_agent(&SessionSlot::lead(&seat.org, &seat.name)))
        .count();
    format!("{live} live / {}", roster.projects.len())
}

/// The state mark the rail and the header draw: the core's lifecycle plus
/// the two promotions the home makes over it.
fn mark_of(state: State) -> &'static str {
    match state {
        State::Lifecycle(SessionLifecycleState::Running | SessionLifecycleState::Spawning) => {
            "live"
        }
        State::Lifecycle(SessionLifecycleState::Idle) => "idle",
        State::Unseen => "unseen",
        State::Lifecycle(SessionLifecycleState::Attention) => "needs",
        // Sign-in needed is a failure the user has to act on, and the rail
        // has no auth shape of its own.
        State::Lifecycle(SessionLifecycleState::AuthRequired | SessionLifecycleState::Failed) => {
            "failed"
        }
        State::Lifecycle(SessionLifecycleState::Sleeping | SessionLifecycleState::LoggedOut)
        | State::NeverStarted => "off",
    }
}

/// The account chip: the account this project would spawn under, opening
/// on what the poller knows about it.
fn account_chip(accounts: &AccountsView, account: Option<&str>) -> Markup {
    let Some(account) = account else {
        return Markup::default();
    };
    let loading = accounts.loading.iter().find(|row| row.display_name == account);
    let usage = accounts.usage_for(account);
    html! {
        details .acct {
            summary { span .dot .idle {} (account) }
            div .pop {
                div .hd {
                    span .nm { (account) }
                    @if let Some(row) = loading {
                        span .st .(state_tone(row.state)) { (state_word(row.state)) }
                    }
                }
                @if let Some(auth) = accounts.auth_for(account) {
                    div .kv { span .k { "type" } span .v { (auth_word(auth)) } }
                }
                @if let Some(usage) = &usage {
                    @for (label, window) in [
                        ("5h", usage.five_hour.as_ref()),
                        ("7d", usage.seven_day.as_ref()),
                    ] {
                        @if let Some(window) = window {
                            div .bar {
                                span .lb { (label) }
                                span .tk {
                                    span .fl style=(format!("width:{}%", window.utilization.clamp(0.0, 100.0))) {}
                                }
                                span .pc { (format!("{:.0}%", window.utilization)) }
                                @if let Some(reset) = &window.reset_description {
                                    span .eta { (reset) }
                                }
                            }
                        }
                    }
                    @if let Some(spend) = &usage.spend {
                        hr;
                        div .kv { span .k { "day" } span .v { (money(spend.daily)) } }
                        div .kv { span .k { "week" } span .v { (money(spend.weekly)) } }
                        div .kv { span .k { "month" } span .v { (money(spend.monthly)) } }
                    }
                    @if let Some(balance) = usage.balance {
                        hr;
                        div .kv {
                            span .k { "balance" }
                            span .v { (money(balance)) " left" }
                        }
                    }
                }
            }
        }
    }
}

fn state_word(state: LoadingState) -> &'static str {
    match state {
        LoadingState::Loading => "probing",
        LoadingState::Ready => "ready",
        LoadingState::Bailed => "bailed",
    }
}

fn state_tone(state: LoadingState) -> &'static str {
    match state {
        LoadingState::Loading => "wait",
        LoadingState::Ready => "ok",
        LoadingState::Bailed => "bad",
    }
}

fn auth_word(auth: AccountAuth) -> &'static str {
    match auth {
        AccountAuth::BaseUrl => "base url",
        AccountAuth::Token => "token",
    }
}

fn money(amount: f64) -> String {
    format!("${amount:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed scan says so, and the two states that are not failures say
    /// nothing. The failure is the one a reader cannot otherwise tell from
    /// a clean tree: the branch draws, no file is listed, and no line
    /// explains the absence.
    ///
    /// A unit test rather than a served page because a fixture cannot make
    /// `git diff --numstat` fail while `git status` succeeds, which is what
    /// reaching this state needs.
    #[test]
    fn a_failed_scan_says_so() {
        assert_eq!(layer_note(&LayerState::ScanFailed), Some("its changes could not be read"));
        assert_eq!(layer_note(&LayerState::Clean), None, "a clean tree has nothing to explain");
        assert_eq!(
            layer_note(&LayerState::Populated(GitDiffStats::default())),
            None,
            "a scan that ran and found nothing is not a failure",
        );
    }
}
