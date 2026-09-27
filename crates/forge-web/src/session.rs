//! The session page: the projects rail, the conversation, and the
//! inspector.
//!
//! Three columns over one sheet. The rail and the inspector collapse to
//! nothing and their handles live in the chat header, so a collapsed pane
//! leaves no edge behind and its control stays reachable. Every column
//! reads the core through the view surface: this module holds no state of
//! its own.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use forge_primitives::Message;
use forge_primitives::PermissionMode;
use forge_primitives::SessionLifecycleState;
use forge_primitives::SessionSlot;
use forge_primitives::account::AccountAuth;
use forge_primitives::git::{GitBranch, GitIssueRef};
use forge_primitives::git_diff::{GitDiffFile, GitDiffSnapshot, GitDiffStats, LayerState};
use forge_primitives::messages::{StopHookInfo, Usage};
use forge_primitives::runtime::RuntimeSessionState;
use forge_primitives::slack::{SlackSubscriptionTarget, SlackWatchMode};
use forge_primitives::tasks::{Task, TaskStatus};
use forge_primitives::{
    ChunkContent, CronEntry, CronKind, McpServerConnectionStatus, McpServerStatus, MonitorRecord,
    MonitorStatus, ToolCallContent,
};
use forge_sessions::family::ToolFamily;
use forge_sessions::grouping::KindRow;
use forge_sessions::model::{
    AnsweredQuestion, LiveTurn, LiveUsage, ToolCallStatus, TurnInfo, format_token_count_grouped,
    format_token_count_short, format_turn_duration,
};
use forge_sessions::surface::connectors::{GotifyView, SlackView};
use forge_sessions::surface::inspector::{
    McpServers, ProcessEntry, ProcessSnapshot, SessionHeader, basename_exe, extract_inner_command,
};
use forge_sessions::surface::{
    AccountsView, Agents, LoadingState, PendingKind, Roster, ViewSurface,
};
use forge_sessions::transcript;
use forge_sessions::transcript::{
    ChatUnit, FamilyLeaves, Notice, NoticeSeverity, PeerCard, ToolLeaf,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde_json::Value;

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

/// The one piece of state a swap would otherwise lose: which sections the
/// reader has opened and closed.
///
/// Idiomorph writes attributes and has no case for `open`, one of them, so a
/// section someone collapsed re-opens on the next swap, and on a working
/// session that is every ten seconds. The state is what the reader decided,
/// kept against the `data-k` each section carries: the region appends, so an
/// element's position among its siblings moves while its key does not.
///
/// Only the reader's own clicks are recorded, and measured against a browser
/// rather than reasoned about: the state cannot be snapshotted before the
/// swap, because htmx's SSE extension swaps its payload itself and fires no
/// `htmx:beforeSwap` for one; and it cannot be read off the `toggle` event,
/// because the swap's own attribute write fires one too, so a section the
/// reader never touched records itself as closed.
const DETAIL_STATE: &str = r"
const decided = new Map();
document.addEventListener('click', (event) => {
  const summary = event.target.closest('summary');
  const section = summary && summary.parentElement;
  if (section instanceof HTMLDetailsElement && section.dataset.k) {
    // The click's own default action flips it a moment after this, so the
    // state worth keeping is the one it lands in rather than the one it
    // left.
    setTimeout(() => decided.set(section.dataset.k, section.open), 0);
  }
}, true);
document.addEventListener('htmx:afterSwap', () => {
  document.querySelectorAll('#session-body details[data-k]').forEach((section) => {
    const want = decided.get(section.dataset.k);
    if (want !== undefined) section.open = want;
  });
});";

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
    let Some(slot) = seat(&state.surface, org, project, label) else {
        return Found::Absent;
    };
    // One walk of the core per page: the roster and the agents the page
    // draws from.
    let roster = state.surface.roster();
    let agents = state.surface.agents();
    let messages = read_conversation(&state.surface, &slot, roster.cwd_for(&slot)).await;
    // The page's first render is before any stream is attached, so it draws
    // no turn row: the stream's opening event follows at once, and it is the
    // connection that holds the clock.
    Found::Page(
        shell(&context(state, bound), &slot, &messages, None, false, &roster, &agents).await,
    )
}

/// True for the frame that says a turn started.
fn is_running_state(msg: &Message) -> bool {
    matches!(
        msg,
        Message::System { subtype, data, .. }
            if subtype == "session_state_changed"
                && forge_sessions::translate::state_parsing::parse_runtime_session_state(
                    data.get("state"),
                ) == Some(RuntimeSessionState::Running)
    )
}

/// The compaction state a streamed frame reports, when it reports one: the
/// session's own status frame carries `compacting` while a compaction runs
/// and a null once it ends. `None` for every other frame.
pub(crate) fn compaction_state(msg: &Message) -> Option<bool> {
    let Message::System { subtype, data, .. } = msg else {
        return None;
    };
    if subtype != "status" {
        return None;
    }
    match data.get("status") {
        Some(serde_json::Value::String(state)) if state == "compacting" => Some(true),
        Some(value) if value.is_null() => Some(false),
        _ => None,
    }
}

/// Fold one streamed message into the live turn: a running state starts it,
/// a settled turn clears it, and an assistant frame adds the input-side
/// counts it carries. Repeat frames for one message overwrite rather than
/// add, which is what keeps a re-sent frame from double-counting.
///
/// The stream is the only writer. The read the connection opens with keeps
/// conversation rows alone, so neither the result that settles a turn nor
/// the state frame that starts one reaches it: a transcript can say what a
/// turn did, and never that one is running.
pub(crate) fn apply_to_live_turn(msg: &Message, live: &mut LiveTurn) {
    match msg {
        Message::System { .. } if is_running_state(msg) => live.start(Instant::now()),
        Message::System { subtype, .. } if subtype == "session_state_changed" => {
            *live = LiveTurn::default();
        }
        Message::Result { .. } => *live = LiveTurn::default(),
        // Summed from the delta, the terminal's own rule: the wire's counter
        // restarts at each thinking block, so the absolute field would step
        // backwards at every new block.
        Message::ThinkingTokens { estimated_tokens_delta, .. } => {
            let delta = u64::try_from(*estimated_tokens_delta).unwrap_or(0);
            live.thinking_tokens = Some(live.thinking_tokens.unwrap_or(0).saturating_add(delta));
        }
        Message::Assistant { message: envelope, .. } => {
            if let Some(usage) = &envelope.usage {
                live.record(envelope.id.clone(), live_usage(usage));
            }
        }
        _ => {}
    }
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
async fn shell(
    home: &Home<'_>,
    slot: &SessionSlot,
    messages: &[Message],
    live_turn: Option<&LiveTurn>,
    compacting: bool,
    roster: &Roster,
    agents: &Agents,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (crate::server::page_head(
                &format!(
                    "forge \u{b7} {} \u{b7} {}",
                    slot.project(),
                    if slot.label() == "lead" { slot.project() } else { slot.label() },
                ),
                home.theme,
                home.font,
            ))
            // The checkboxes are the pane state: CSS-only, so a collapsed
            // pane needs no script. Checked is COLLAPSED.
            //
            // The stream is wired by attributes, as the home's is: htmx
            // opens it, swaps the `session` event's payload into the region,
            // and closes on the server's own `close` event. The listener
            // sits on a wrapper the payload never replaces, because htmx
            // re-processes what it swaps in and a listener on the region
            // itself would register one more per event. The checkboxes sit
            // on that wrapper too, beside the region rather than inside it:
            // the rules that read them are sibling combinators, so a box
            // the swap replaced would take every one of them with it.
            body hx-ext="sse, morph" sse-connect=(events_path(slot)) sse-close="close" {
                (icons::sprite())
                div #live sse-swap="session" hx-swap="morph:outerHTML" hx-target="#session-body" {
                    input type="checkbox" id="l" hidden;
                    input type="checkbox" id="r" hidden;
                    (columns(home, slot, messages, live_turn, compacting, roster, agents).await)
                }
                script src="/vendor/htmx.js" {}
                script src="/vendor/htmx-sse.js" {}
                script src="/vendor/idiomorph.js" {}
                script { (PreEscaped(DETAIL_STATE)) }
            }
        }
    }
}

/// The region the stream swaps in: the same markup the page opened with,
/// drawn from the conversation the connection holds rather than a fresh
/// read, because a read per update is a disk walk per update.
pub(crate) async fn session_region(
    state: &WebState,
    bound: SocketAddr,
    slot: &SessionSlot,
    conversation: &[Message],
    live_turn: Option<&LiveTurn>,
    compacting: bool,
) -> Markup {
    let home = context(state, bound);
    let roster = state.surface.roster();
    let agents = state.surface.agents();
    columns(&home, slot, conversation, live_turn, compacting, &roster, &agents).await
}

/// The seat a route names, when the roster holds it. A project's own lead
/// seat exists whether or not it has ever run; a worker's exists only while
/// the roster can name it.
pub(crate) fn seat(
    surface: &ViewSurface,
    org: &str,
    project: &str,
    label: &str,
) -> Option<SessionSlot> {
    let roster = surface.roster();
    let agents = surface.agents();
    let found = roster.projects.iter().find(|seat| seat.org == org && seat.name == project)?;
    if label == "lead" {
        return Some(SessionSlot::lead(org, project));
    }
    let named = agents.for_project(&found.key).iter().any(|row| row.label == label)
        || surface.workers().for_project(&found.key).iter().any(|row| row.label == label);
    named.then(|| SessionSlot::worker(org, project, label))
}

/// The page's three columns, which the first render and every swap both
/// draw.
async fn columns(
    home: &Home<'_>,
    slot: &SessionSlot,
    messages: &[Message],
    live_turn: Option<&LiveTurn>,
    compacting: bool,
    roster: &Roster,
    agents: &Agents,
) -> Markup {
    let units = transcript::render_units(messages);
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
    // What the conversation draws after its last block: a compaction in
    // flight, and the row of the turn that is running. `None` when there is
    // neither, so nothing opens a block for an empty tail.
    let live = live_turn.filter(|live| live.started_at.is_some());
    let tail = (compacting || live.is_some()).then(|| {
        html! {
            @if compacting {
                div .compacting { span .ring {} "Compacting context\u{2026}" }
            }
            @if let Some(live) = live {
                (turn_report_row(&live_report(live, Instant::now()), true, "turn-live"))
            }
        }
    });
    let header = home.surface.header(slot);

    html! {
    div #session-body {
        div .app {
                aside .rail .left {
                    div .banner {
                        span .t { "projects" }
                        span .n .ml { (fleet_count(roster)) }
                        // A pane covering the page carries its own way out:
                        // the header handle that opened it is underneath.
                        label .close for="l" title="close" { "\u{d7}" }
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
                        (chat_body(
                            waking,
                            row.and_then(|row| row.reason.as_deref()),
                            &units,
                            roster.cwd_for(slot).as_deref(),
                            tail.as_ref(),
                        ))
                    }
                }
                aside .rail .right {
                    div .banner {
                        span .t { "inspector" }
                        span .n .ml { (slot.project()) }
                        label .close for="r" title="close" { "\u{d7}" }
                    }
                    div .scroll { (inspector(home, roster, slot).await) }
                }
            }
        }
    }
}

/// The page's own stream: a slot's region, one connection per tab.
fn events_path(slot: &SessionSlot) -> String {
    format!("{}/events", href(slot))
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
        div .note { "The only surface subagents have - the chat suppresses them." }
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
                span .k { (server.name) " \u{b7} " (scope_label(server)) }
                span .v { (mcp_state(server)) }
            }
        }
        @if let Some(error) = &servers.error {
            div .note { (error) }
        }
    };
    section(false, "mcp", "mcp servers", &summary, &body)
}

/// The scope a server is configured in, which is what its config blob names
/// rather than where its process runs. The terminal's own row reads it the
/// same way, so a server that reports no scope reads as the session's.
fn scope_label(server: &McpServerStatus) -> String {
    if let Some(scope) = server.scope.as_deref() {
        return scope.to_owned();
    }
    match server.config.as_ref().and_then(|config| config.get("type")).and_then(Value::as_str) {
        Some("sdk") => "sdk".to_owned(),
        _ => "session".to_owned(),
    }
}

/// What the row says in its value column: how many tools the server offers
/// when it is up, and why it is not when it is not. The rule is the
/// terminal's own; the words are this view's, and two of the five read
/// differently because a page has room for them and a pane of rows does
/// not.
fn mcp_state(server: &McpServerStatus) -> String {
    match server.status {
        McpServerConnectionStatus::Connected => server
            .tools
            .as_ref()
            .map_or_else(|| "connected".to_owned(), |tools| tool_summary(tools.len())),
        McpServerConnectionStatus::Failed => server
            .error
            .as_deref()
            .map(str::trim)
            .filter(|error| !error.is_empty())
            .unwrap_or("failed")
            .to_owned(),
        McpServerConnectionStatus::NeedsAuth => "needs sign-in".to_owned(),
        McpServerConnectionStatus::Pending => "connecting".to_owned(),
        McpServerConnectionStatus::Disabled => "disabled".to_owned(),
    }
}

/// How many tools a server offers, counted so that one reads as one.
fn tool_summary(count: usize) -> String {
    match count {
        0 => "no tools".to_owned(),
        1 => "1 tool".to_owned(),
        count => format!("{count} tools"),
    }
}

/// The processes section: claude's descendant tree, a row each, with the
/// tree's shape carried by the row's indent.
fn processes_section(walk: &ProcessSnapshot) -> Markup {
    let count = walk.processes.len();
    let tree = process_tree(walk);
    let walked = walked_note(walk.scanned_at);
    let body = html! {
        @for (entry, depth) in tree {
            div .kv {
                span .k {
                    @for _ in 0..depth { (PreEscaped("&nbsp;&nbsp;")) }
                    (process_headline(entry))
                }
                span .v { (format!("{} \u{b7} {}", memory_label(entry.memory_bytes), entry.pid)) }
            }
        }
        div .note { (walked) }
    };
    section(false, "processes", "processes", &count.to_string(), &body)
}

/// What a row calls its process: the command it is running, with the
/// executable's path stripped, and the command a shell wrapper wraps rather
/// than its own chrome. The OS name alone says nothing about which node
/// process it is, which is why the terminal's own row draws this.
fn process_headline(entry: &ProcessEntry) -> String {
    let command = entry.command.trim();
    if let Some(inner) = extract_inner_command(&entry.command) {
        return basename_exe(&inner);
    }
    if command.is_empty() {
        return if entry.name.is_empty() { "(process)".to_owned() } else { entry.name.clone() };
    }
    basename_exe(command)
}

/// When the walk behind these rows was taken. The walk is only ever
/// performed for the session a view is looking at, so a slot nobody is
/// looking at serves the last tree left on it, and rows from an hour ago
/// drawn exactly like rows from a second ago would be a wrong answer
/// rather than an old one.
fn walked_note(scanned_at: SystemTime) -> String {
    match crate::home::elapsed_label(scanned_at).as_str() {
        "now" => "walked just now".to_owned(),
        age => format!("walked {age} ago"),
    }
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
                        (icons::icon("check", "st"))
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
        details .sec open[open] data-k=(format!("sec-{name}")) {
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

/// What the chat column draws: the conversation, or the seat's own state
/// when there is none to draw.
///
/// It claims nothing about a spawn. This page cannot start a session, so a
/// line saying one is coming would be a promise no code keeps, and a seat
/// whose spawn failed would carry a failure mark above a line saying it is
/// connecting. A seat that is up with nothing said yet draws no skeleton
/// either: an empty conversation is empty.
fn chat_body(
    waking: bool,
    reason: Option<&str>,
    units: &[ChatUnit],
    cwd: Option<&Path>,
    tail: Option<&Markup>,
) -> Markup {
    if waking {
        return html! {
            div .hold .off {
                "not running"
                span .sub { (reason.unwrap_or("this seat has no session behind it")) }
            }
        };
    }
    conversation(units, cwd, tail)
}

/// A session's conversation, read off the reactor: the read walks a whole
/// transcript on the calling thread, and a handler that waits for it stalls
/// every other request.
pub(crate) async fn read_conversation(
    surface: &Arc<ViewSurface>,
    slot: &SessionSlot,
    cwd: Option<PathBuf>,
) -> Vec<Message> {
    let surface = Arc::clone(surface);
    let reading = slot.clone();
    let cwd = cwd.unwrap_or_default();
    let read =
        tokio::task::spawn_blocking(move || surface.conversation(&reading, &cwd).messages).await;
    match read {
        Ok(messages) => messages,
        // An empty conversation and a read that never happened draw the same
        // page, so the failure is the one thing that has to say which it was.
        Err(err) => {
            tracing::warn!(org = slot.org(), project = slot.project(), label = slot.label(), %err,
                event_name = "web_conversation_read_failed",
                "the session page could not read the conversation; drawing it empty");
            Vec::new()
        }
    }
}

/// The conversation, as the fold's units read: the user's own turns on their
/// own, and everything the assistant did in one work block after each.
fn conversation(units: &[ChatUnit], cwd: Option<&Path>, tail: Option<&Markup>) -> Markup {
    let rows = Cell::new(0usize);
    let turns = turns(units);
    // The tail is the live turn's row and the compaction line, and it draws
    // inside the last work block, because that is where the settled row lands
    // when the turn ends: a block of its own sits a block padding lower, and
    // the row would move under the reader at the settle. With no tail there is
    // no block to open for one, which is what keeps an empty conversation from
    // drawing a block of nothing.
    let last_work = tail.and(turns.iter().rposition(|turn| matches!(turn, Turn::Work(_))));
    let tail_alone = tail.is_some() && last_work.is_none();
    html! {
        @for (at, turn) in turns.iter().enumerate() {
            @match turn {
                Turn::Mine(text) => div .mine { (text) },
                Turn::Work(work) => div .work {
                    @for unit in work {
                        (unit_markup(unit, cwd, &rows))
                    }
                    @if Some(at) == last_work {
                        @if let Some(tail) = tail {
                            (tail)
                        }
                    }
                },
            }
        }
        @if tail_alone {
            @if let Some(tail) = tail {
                div .work { (tail) }
            }
        }
    }
}

/// The units as turns. A user turn stands alone; a stretch of anything else
/// belongs to the work block it sits in.
enum Turn<'a> {
    Mine(&'a str),
    Work(Vec<&'a ChatUnit>),
}

fn turns(units: &[ChatUnit]) -> Vec<Turn<'_>> {
    let mut turns: Vec<Turn<'_>> = Vec::new();
    for unit in units {
        match unit {
            ChatUnit::UserTurn { text } => turns.push(Turn::Mine(text)),
            other => match turns.last_mut() {
                Some(Turn::Work(work)) => work.push(other),
                _ => turns.push(Turn::Work(vec![other])),
            },
        }
    }
    turns
}

/// One unit of work.
fn unit_markup(unit: &ChatUnit, cwd: Option<&Path>, rows: &Cell<usize>) -> Markup {
    match unit {
        ChatUnit::AssistantText { text } => html! { div .prose { (prose(text)) } },
        ChatUnit::ToolGroup { families, status } => tool_group(families, *status, cwd),
        ChatUnit::QuestionCard { asked } => question_card(asked),
        ChatUnit::PeerCard(card) => peer_card(card),
        ChatUnit::MessagingGroup { cards } => messaging_group(cards),
        ChatUnit::Notice(notice) => notice_row(notice),
        ChatUnit::Hooks { key, actions, infos } => hooks_row(key, *actions, infos),
        ChatUnit::TurnReport(info) => {
            let nth = rows.get();
            rows.set(nth + 1);
            // A settled row is named for its place in the conversation: it
            // carries no id of its own, and the rows before it do not move.
            turn_report_row(info, false, &format!("turn-{nth}"))
        }
        // A user turn is the block around its work, drawn by `conversation`.
        ChatUnit::UserTurn { text } => html! { div .mine { (text) } },
    }
}

/// The assistant's prose, as markdown. The angle brackets go in escaped:
/// markdown passes raw HTML through, and a session's prose can quote
/// anything it read, which the page would then run.
fn prose(text: &str) -> Markup {
    let escaped = text.replace('<', "&lt;");
    let parser = pulldown_cmark::Parser::new(&escaped);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    PreEscaped(html)
}

/// The mark a call or a run carries: a check when it came back, a cross when
/// it failed, and the ring while it is still out.
fn status_icon(status: ToolCallStatus) -> Markup {
    match status {
        ToolCallStatus::Completed => icons::icon("check", "st"),
        ToolCallStatus::Failed | ToolCallStatus::Killed => icons::icon("x", "st err"),
        ToolCallStatus::Pending | ToolCallStatus::InProgress => {
            html! { span .st { span .ring {} } }
        }
    }
}

/// One run of calls: the count and the run's own status, then a label per
/// family with its calls indented under it.
fn tool_group(families: &[FamilyLeaves], status: ToolCallStatus, cwd: Option<&Path>) -> Markup {
    let calls: usize = families.iter().map(|family| family.calls.len()).sum();
    // The run keeps the key of the call it opened with: the calls after it
    // are appended, so what identifies the run does not move.
    let first = families.iter().find_map(|family| family.calls.first());
    let key = format!("kind-{}", first.map_or("empty", |call| call.id.as_str()));
    html! {
        details .kind open data-k=(key) {
            summary {
                (status_icon(status))
                span .nm { (calls) " tool " @if calls == 1 { "call" } @else { "calls" } }
                (icons::chevron(""))
            }
            div .leaves {
                @for family in families {
                    div .knd {
                        (icons::icon(family_icon(family.row), "gl"))
                        span .nm { (&family.label) }
                    }
                    @for call in &family.calls {
                        (leaf_row(call, opens_by_default(family.row), cwd))
                    }
                }
            }
        }
    }
}

/// One call: its own status and title, opening on its body. A mutation opens
/// by default - the mockup draws an edit's diff already there - and every
/// other call waits to be asked.
fn leaf_row(leaf: &ToolLeaf, open: bool, cwd: Option<&Path>) -> Markup {
    html! {
        details .leaf open[open] data-k=(format!("leaf-{}", leaf.id)) {
            summary {
                (status_icon(leaf.status))
                span .tn { (call_target(leaf, cwd)) }
                (icons::chevron(""))
            }
            @if !leaf.content.is_empty() {

                div .body { (leaf_body(leaf, &leaf.content)) }
            }
        }
    }
}

/// What a call's row opens on.
/// A turn's hooks, as the mockup draws them: the count ahead of a chip that
/// opens on what each hook ran and how long it took.
fn hooks_row(key: &str, actions: u32, infos: &[StopHookInfo]) -> Markup {
    html! {
        details .hooks data-k=(format!("hooks-{key}")) {
            summary {
                "\u{21B3} hook summary \u{b7} " (actions) " actions"
                span .tog {}
            }
            @if !infos.is_empty() {
                div .body {
                    @for info in infos {
                        div .term {
                            (info.command)
                            @if let Some(ms) = info.duration_ms {
                                " \u{b7} " (format_turn_duration(ms))
                            }
                        }
                    }
                }
            }
        }
    }
}

fn leaf_body(leaf: &ToolLeaf, content: &[ToolCallContent]) -> Markup {
    html! {
        @for content in content {
            @match content {
                ToolCallContent::Diff { new_path, old, new, .. } => {
                    (diff_body(new_path, old, new))
                }
                ToolCallContent::Content { content } => {
                    @match content {
                        ChunkContent::Text { text } => (text_body(leaf, text)),
                        other => (chunk_body(other)),
                    }
                }
                ToolCallContent::McpResource { text, uri, .. } => {
                    div .term { (text.clone().unwrap_or_else(|| uri.clone())) }
                }
            }
        }
    }
}

/// A call's text body in the shape the mockup draws for it: a source file
/// as a highlighted code block, a search result as one row per hit, and
/// anything else as plain terminal rows.
fn text_body(leaf: &ToolLeaf, text: &str) -> Markup {
    if let Some(hits) = search_hits(leaf, text) {
        return hits;
    }
    // The language comes from a call that named a file. A command's title is
    // the description it was called with, which can end in an extension
    // without being a path, and reading it as one drew the command's output
    // as code and left the command itself nowhere.
    if leaf.label == "Read"
        && let Some(language) = language_of(&leaf.title)
    {
        code_body(language, text)
    } else {
        html! {
            div .term {
                // The command a call ran leads its output: a call with a
                // description shows that as its title, so this is the only
                // place the command itself is drawn.
                @if let Some(command) = &leaf.command {
                    span .pfx { "$" }
                    " " (command) "\n"
                }
                (text)
            }
        }
    }
}

/// The body of a search call, when that is what this is: every hit the call
/// came back with, one row each. The wire's shape is `path:line:content`,
/// which is what the mockup draws as a line number, the file, and the line
/// the match sits in. `None` when the call is not a search or the text is
/// not that shape, which leaves the body to the terminal rows.
fn search_hits(leaf: &ToolLeaf, text: &str) -> Option<Markup> {
    let search = matches!(leaf.label, "Grep" | "Glob" | "LS");
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    if !search || lines.is_empty() || !lines.iter().all(|line| split_hit(line).is_some()) {
        return None;
    }
    let pattern = leaf.title.clone();
    Some(html! {
        @for line in lines.iter().take(20) {
            @if let Some((path, number, rest)) = split_hit(line) {
                div .searchhit {
                    span .ln { (number) ":" } " " span .fl { (path) }
                    @if !rest.is_empty() {
                        "\n" (emphasised(&pattern, rest))
                    }
                }
            }
        }
        @if lines.len() > 20 {
            div .searchhit { "\u{2026}and " (lines.len() - 20) " more" }
        }
    })
}

/// One hit line as `path:line:content`. `None` for a line that is not that
/// shape, which is what makes the whole body something else.
fn split_hit(line: &str) -> Option<(&str, &str, &str)> {
    let (path, rest) = line.split_once(':')?;
    if path.is_empty() || path.contains(' ') {
        return None;
    }
    let (number, content) = rest.split_once(':').unwrap_or((rest, ""));
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((path, number, content.trim_start()))
}

/// The hit's own line with the search pattern marked, which is what the
/// mockup's `.hit` span is for.
fn emphasised(pattern: &str, line: &str) -> Markup {
    if pattern.is_empty() || !line.contains(pattern) {
        return html! { (line) };
    }
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find(pattern) {
        let (before, after) = rest.split_at(at);
        out.push_str(&escaped(before));
        out.push_str("<span class=\"hit\">");
        out.push_str(&escaped(pattern));
        out.push_str("</span>");
        rest = &after[pattern.len()..];
    }
    out.push_str(&escaped(rest));
    PreEscaped(out)
}

/// Text as the page carries it: the highlighter builds its own markup, so
/// what it wraps is escaped here rather than by the macro.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A source file, highlighted into the four token classes the mockup draws.
/// A file the highlighter has no syntax for, or a line it cannot parse, is
/// drawn as it came: the page never invents text a reader cannot use.
fn code_body(language: &str, text: &str) -> Markup {
    let syntaxes = syntaxes();
    let Some(syntax) = syntaxes.find_syntax_by_token(language) else {
        return html! { div .term { (text) } };
    };
    let mut state = syntect::parsing::ParseState::new(syntax);
    let mut body = String::new();
    for line in text.lines() {
        let spans = state.parse_line(line, syntaxes).unwrap_or_default();
        let mut at = 0;
        let mut stack = syntect::parsing::ScopeStack::new();
        for (offset, op) in spans {
            if offset > at {
                push_span(&mut body, token_class(&stack), &line[at..offset]);
                at = offset;
            }
            let _ = stack.apply(&op);
        }
        push_span(&mut body, token_class(&stack), &line[at..]);
        body.push('\n');
    }
    html! {
        div .code {
            div .lang { (language) }
            pre { (PreEscaped(body)) }
        }
    }
}

/// One run of the line, in the class its scopes name, escaped for the page.
fn push_span(out: &mut String, class: Option<&str>, text: &str) {
    match class {
        Some(class) => {
            out.push_str("<span class=\"");
            out.push_str(class);
            out.push_str("\">");
            out.push_str(&escaped(text));
            out.push_str("</span>");
        }
        None => out.push_str(&escaped(text)),
    }
}

/// The class a scope stack draws in: a comment, a string, a keyword, or a
/// name that is a function or a type. Everything else is plain.
///
/// Read from the innermost scope out, which is the one a token's own nature
/// is stated in: a keyword inside a string is a string.
fn token_class(stack: &syntect::parsing::ScopeStack) -> Option<&'static str> {
    for scope in stack.as_slice().iter().rev() {
        let name = scope.build_string();
        if name.starts_with("comment") {
            return Some("c2");
        }
        if name.contains("string") {
            return Some("s");
        }
        if name.contains("keyword") || name.starts_with("storage") {
            return Some("k");
        }
        if name.starts_with("entity.name.function")
            || name.starts_with("entity.name.type")
            || name.starts_with("support.type")
            || name.starts_with("entity.name.tag")
        {
            return Some("f");
        }
    }
    None
}

/// The syntax definitions, loaded once for the process: they are a few
/// megabytes of tables, and a page renders on every swap.
fn syntaxes() -> &'static syntect::parsing::SyntaxSet {
    static SYNTAXES: std::sync::OnceLock<syntect::parsing::SyntaxSet> = std::sync::OnceLock::new();
    SYNTAXES.get_or_init(syntect::parsing::SyntaxSet::load_defaults_newlines)
}

/// The language a path's extension names, when the page has a highlighter
/// for it. Docs and plain text are not source and keep the terminal rows.
fn language_of(path: &str) -> Option<&'static str> {
    let extension = path.rsplit_once('.')?.1;
    Some(match extension {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "sh" | "bash" | "zsh" => "bash",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "css" => "css",
        "html" => "html",
        "sql" => "sql",
        _ => return None,
    })
}

/// A text chunk: the terminal rows the mockup draws for a command's output.
/// An image chunk is a body this page has no renderer for, so it says what
/// it is rather than drawing nothing.
fn chunk_body(content: &ChunkContent) -> Markup {
    match content {
        ChunkContent::Text { text } => html! { div .term { (text) } },
        ChunkContent::Image { mime_type, uri, .. } => html! {
            div .term {
                "image"
                @if let Some(mime) = mime_type { " \u{b7} " (mime) }
                @if let Some(uri) = uri { " \u{b7} " (uri) }
            }
        },
    }
}

/// A mutation, as the mockup draws one: the removed lines then the added
/// ones, numbered. The scan hands whole strings rather than a hunk, so the
/// body says what changed without inventing the diff's own offsets.
fn diff_body(path: &str, old: &str, new: &str) -> Markup {
    html! {
        div .dif {
            div .h { (path) }
            @for line in old.lines() {
                div .ln .d { span .n { "\u{2212}" } span .l { (line) } }
            }
            @for line in new.lines() {
                div .ln .a { span .n { "+" } span .l { (line) } }
            }
        }
    }
}

/// The icon a family row draws: the family's own sprite, and the generic
/// tool's for anything this page has no icon for.
fn family_icon(row: KindRow) -> &'static str {
    match row {
        KindRow::Mcp => "mcp",
        KindRow::Inbound | KindRow::Outbound => "in",
        KindRow::Family(family) => match family {
            ToolFamily::Read => "read",
            ToolFamily::Search => "search",
            ToolFamily::Bash => "bash",
            ToolFamily::Web => "web",
            ToolFamily::Lsp => "lsp",
            ToolFamily::Skill => "skill",
            ToolFamily::ToolSearch => "toolsearch",
            // The fold gives every mutation this one row, so a view names it
            // once rather than listing the four tools behind it.
            ToolFamily::Own("edit") => "edit",
            ToolFamily::Config | ToolFamily::Worktree | ToolFamily::Tool | ToolFamily::Own(_) => {
                "tool"
            }
        },
    }
}

/// Whether a family's calls start open. A mutation's diff is what a reader
/// came for, and the mockup draws it without being asked.
fn opens_by_default(row: KindRow) -> bool {
    matches!(row, KindRow::Family(ToolFamily::Own("edit")))
}

/// A call's title without the family word the row above already says, and
/// without the working directory the reader is already in: the mockup draws
/// `crates/.../family.rs` under a `read` label.
fn call_target(leaf: &ToolLeaf, cwd: Option<&Path>) -> String {
    let title = leaf
        .title
        .strip_prefix(leaf.label)
        .map_or(leaf.title.clone(), |rest| rest.trim_start().to_owned());
    let Some(cwd) = cwd.and_then(|cwd| cwd.to_str()) else {
        return title;
    };
    match title.strip_prefix(cwd).and_then(|rest| rest.strip_prefix('/')) {
        Some(relative) if !relative.is_empty() => relative.to_owned(),
        _ => title,
    }
}

/// A question the assistant asked and a person answered: the question, what
/// was picked, and what was typed, each on its own line.
fn question_card(asked: &[AnsweredQuestion]) -> Markup {
    html! {
        @for pair in asked {
            div .card {
                div .q { (icons::icon("question", "qm")) (pair.question) }
                @for picked in &pair.picked_labels {
                    div .a {
                        span .am { "\u{2192}" }
                        span .picked { (picked) }
                    }
                }
                @if let Some(typed) = &pair.typed_note {
                    div .a {
                        span .am { "\u{2192}" }
                        span .am { "you typed:" }
                        span .typed { "\u{201C}" (typed) "\u{201D}" }
                    }
                }
            }
        }
    }
}

/// One peer message, on its own: the direction, who it was, and what it
/// said.
fn peer_card(card: &PeerCard) -> Markup {
    html! {
        div .peer {
            (icons::icon(if card.inbound { "in" } else { "out" }, "dir"))
            span .from { (&card.peer) }
            span .txt { (first_line(&card.body)) }
        }
    }
}

/// A run of two or more peer messages: a count over one row per message,
/// each with its own kind and direction.
fn messaging_group(cards: &[PeerCard]) -> Markup {
    let key = cards
        .first()
        .map_or_else(|| "msg-empty".to_owned(), |card| format!("msg-{}-{}", card.peer, card.body));
    html! {
        details .msg open data-k=(key) {
            summary {
                (icons::icon("check", "st"))
                span .c { (cards.len()) " messages" }
                (icons::chevron(""))
            }
            div .msgbody {
                @for card in cards {
                    div .mmsg {
                        (icons::icon(if card.inbound { "in" } else { "out" }, "dir"))
                        span .kb { (card.kind) }
                        span .who { (&card.peer) }
                        span .txt { (first_line(&card.body)) }
                    }
                }
            }
        }
    }
}

/// A line nobody typed: an external delivery, a scheduled fire, or a failure
/// the workspace reported.
fn notice_row(notice: &Notice) -> Markup {
    let (class, severity) = match notice.severity {
        NoticeSeverity::Info => ("notice info", "Info"),
        NoticeSeverity::Warning => ("notice warn", "Warning"),
        NoticeSeverity::Error => ("notice err", "Error"),
    };
    html! {
        div class=(class) {
            span .sev { (severity) }
            (notice.text.as_str())
        }
    }
}

/// The first line of a body the row clips: a card's row is one line by the
/// mockup's own rule, and a message can be any length.
fn first_line(text: &str) -> String {
    text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default().to_owned()
}

/// A settled turn's row: what it did, under the work it did it with. The row
/// and its body draw the record the fold read from the CLI's own frame, so
/// both fields a view shows are the shipped ones rather than a second
/// reading of the wire.
///
/// The collapsed row drops a field it does not have; the body holds its place
/// with a dash. Neither ever writes a zero for an absent value: the CLI
/// attributing nothing arrives as a zero block, and a zero here reads as a
/// measurement.
fn turn_report_row(info: &TurnInfo, live: bool, key: &str) -> Markup {
    let info = attributed_usage(info);
    let info = &info;
    html! {
        details .turninfo data-k=(key) {
            summary {
                @if live {
                    span .ring {}
                } @else {
                    span { "\u{21A9}" }
                }
                span { (format_turn_duration(info.elapsed_ms())) }
                // The estimate belongs to a turn still running: once the
                // result lands its billed counts take the row, and the body
                // is where the estimate stays.
                @if let Some(thinking) = live.then_some(info.thinking_tokens).flatten() {
                    span .sep { "\u{b7}" }
                    span { "thinking " (format_token_count_short(thinking)) }
                }
                @if let Some(tokens) = turn_token_field(info) {
                    span .sep { "\u{b7}" }
                    span { (tokens) }
                }
                @if let Some(pct) = info.cache_hit_percent() {
                    span .sep { "\u{b7}" }
                    span { (pct) "% cached" }
                }
                @if let Some(written) = info.cache_written_tokens {
                    span .sep { "\u{b7}" }
                    span { (format_token_count_short(written)) " written" }
                }
                @if let Some(cost) = info.session_cost_usd {
                    span .sep { "\u{b7}" }
                    span { (money(cost)) " cumulative" }
                }
                span .tog {}
            }
            div .tibody { (turn_body(info)) }
        }
    }
}

/// The body behind an expanded row, in the mockup's two columns. A cell with
/// nothing behind it is a dash, except the cache sentence, which is dropped
/// rather than reading as broken.
fn turn_body(info: &TurnInfo) -> Markup {
    let dash = || "-".to_owned();
    html! {
        span .l { b { "ended" } (info.ended_at_local.clone().unwrap_or_else(dash)) }
        span .n { b { "model" } (info.model.clone().unwrap_or_else(dash)) }
        span .l { b { "elapsed" } (format_turn_duration(info.elapsed_ms())) }
        span .n {
            b { "api" }
            (info.api_ms.map_or_else(dash, format_turn_duration))
        }
        span .l {
            b { "local" }
            @match info.local_ms() {
                Some(local) => { (format_turn_duration(local)) " tools + hooks" }
                None => { "-" }
            }
        }
        span .n {}
        span .l {
            b { "thinking" }
            @match info.thinking_tokens {
                Some(tokens) => { (format_token_count_grouped(tokens)) " est" }
                None => { "-" }
            }
        }
        span .n {}
        span .l { b { "in" } (info.input_tokens.map_or_else(dash, format_token_count_grouped)) }
        span .n { b { "out" } (info.output_tokens.map_or_else(dash, format_token_count_grouped)) }
        span .l {
            b { "cache" }
            @match info.cache_read_tokens {
                Some(read) => { (format_token_count_grouped(read)) " read" }
                None => { "-" }
            }
        }
        span .n {
            b { "wrote" }
            (info.cache_written_tokens.map_or_else(dash, format_token_count_grouped))
        }
        @if let Some(pct) = info.cache_hit_percent() {
            span .n .wide { (pct) "% of input served from cache" }
        }
        span .l {
            b { "session" }
            @match info.session_cost_usd {
                Some(cost) => { (money(cost)) " cumulative" }
                None => { "-" }
            }
        }
        span .n {}
    }
}

/// The record with an unattributed usage block dropped, which is the rule the
/// TUI applies before it stamps a turn: one filter over the whole block, and
/// every counter written through as it came.
///
/// The distinction matters at a single counter. A frame whose counters are
/// all zero is the CLI saying it has nothing to attribute, and a compaction
/// result is that shape; a real zero inside a block that does carry counters
/// is a measurement, and prints as one.
fn attributed_usage(info: &TurnInfo) -> TurnInfo {
    let nothing = info.input_tokens.unwrap_or(0) == 0
        && info.output_tokens.unwrap_or(0) == 0
        && info.cache_read_tokens.unwrap_or(0) == 0
        && info.cache_written_tokens.unwrap_or(0) == 0;
    if nothing {
        TurnInfo {
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_written_tokens: None,
            ..info.clone()
        }
    } else {
        info.clone()
    }
}

/// The `4.2k↑ 1.1k↓` pair, or just the input side while the turn is still
/// running: a mid-turn output count is a streaming placeholder rather than a
/// count, so there is nothing to show yet.
fn turn_token_field(info: &TurnInfo) -> Option<String> {
    let input = format_token_count_short(info.input_tokens?);
    match info.output_tokens {
        Some(output) => {
            Some(format!("{input}\u{2191} {}\u{2193}", format_token_count_short(output)))
        }
        None => Some(format!("{input}\u{2191}")),
    }
}

/// The record a turn still in flight can honestly report: its clock, and the
/// input-side counts its frames have carried. Output and cost are absent by
/// design - an assistant frame's output count is a streaming placeholder,
/// and the cost arrives with the result - so the collapsed row drops them and
/// the body holds their places with dashes, which is what keeps the body the
/// same height across the settle.
fn live_report(live: &LiveTurn, now: Instant) -> TurnInfo {
    let totals = live.totals();
    TurnInfo {
        started_at: live.started_at,
        elapsed_secs: live
            .started_at
            .map_or(0, |started| now.saturating_duration_since(started).as_secs()),
        thinking_tokens: live.thinking_tokens,
        input_tokens: totals.map(|usage| usage.input_tokens),
        cache_read_tokens: totals.map(|usage| usage.cache_read_tokens),
        cache_written_tokens: totals.map(|usage| usage.cache_written_tokens),
        ..TurnInfo::default()
    }
}

/// A frame's input-side usage, in the shape the live turn accumulates. The
/// output side is deliberately not read.
fn live_usage(usage: &Usage) -> LiveUsage {
    LiveUsage {
        input_tokens: usage.input_tokens,
        cache_read_tokens: usage.cache_read_input_tokens,
        cache_written_tokens: usage.cache_creation_input_tokens,
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
        details .acct data-k="acct" {
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
