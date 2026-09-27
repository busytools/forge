//! The session page: the projects rail, the conversation, and the
//! inspector.
//!
//! Three columns over one sheet. The rail and the inspector collapse to
//! nothing and their handles live in the chat header, so a collapsed pane
//! leaves no edge behind and its control stays reachable. Every column
//! reads the core through the view surface: this module holds no state of
//! its own.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use forge_primitives::Message;
use forge_primitives::SessionLifecycleState;
use forge_primitives::SessionSlot;
use forge_primitives::account::AccountAuth;
use forge_primitives::git::{GitBranch, GitIssueRef};
use forge_primitives::git_diff::{GitDiffFile, GitDiffSnapshot, GitDiffStats, LayerState};
use forge_primitives::messages::Usage;
use forge_primitives::runtime::RuntimeSessionState;
use forge_primitives::slack::{SlackSubscriptionTarget, SlackWatchMode};
use forge_primitives::tasks::{Task, TaskStatus};
use forge_primitives::{ChunkContent, CronEntry, CronKind, ToolCallContent};
use forge_sessions::family::ToolFamily;
use forge_sessions::grouping::KindRow;
use forge_sessions::model::{AnsweredQuestion, ToolCallStatus};
use forge_sessions::surface::connectors::{GotifyView, SlackView};
use forge_sessions::surface::{
    AccountsView, Agents, LoadingState, PendingKind, Roster, ViewSurface,
};
use forge_sessions::transcript;
use forge_sessions::transcript::{
    ChatUnit, FamilyLeaves, Notice, NoticeSeverity, PeerCard, ToolLeaf,
};
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
    let Some(slot) = seat(&state.surface, org, project, label) else {
        return Found::Absent;
    };
    // One walk of the core per page: the roster and the agents the page
    // draws from.
    let roster = state.surface.roster();
    let agents = state.surface.agents();
    let messages = read_conversation(&state.surface, &slot, roster.cwd_for(&slot)).await;
    // The page's first render is before any stream is attached, so it has no
    // turn clock of its own: the stream's opening event follows at once and
    // carries one when a turn is in flight.
    let turn_started = turn_in_flight(&messages);
    Found::Page(
        shell(&context(state, bound), &slot, &messages, turn_started, &roster, &agents).await,
    )
}

/// Arm or disarm a turn clock from a message that reports a session state:
/// the running state arms it, and a settled turn disarms it.
///
/// The clock is set when the message arrives, not when the page renders: a
/// clock read at render time always says the turn began just now.
pub(crate) fn arm_turn_clock(msg: &Message, started: &mut Option<SystemTime>) {
    match msg {
        Message::System { subtype, data, .. } if subtype == "session_state_changed" => {
            *started = match forge_sessions::translate::state_parsing::parse_runtime_session_state(
                data.get("state"),
            ) {
                Some(RuntimeSessionState::Running) => Some(SystemTime::now()),
                _ => None,
            };
        }
        Message::Result { .. } => *started = None,
        _ => {}
    }
}

/// Whether the conversation the page just read ends on a turn still running.
/// A page opened mid-turn draws its turn row from the moment it attached,
/// because a turn's start is not in the transcript: the count is honest
/// about what it measures, and the next turn's is exact.
pub(crate) fn turn_in_flight(messages: &[Message]) -> Option<SystemTime> {
    let mut started = None;
    for msg in messages {
        arm_turn_clock(msg, &mut started);
    }
    started
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
    }
}

/// The page. One page for both outcomes: the columns are as real for a
/// seat nothing is running behind as for one that is up, and only the chat
/// column says which of the two it is looking at.
async fn shell(
    home: &Home<'_>,
    slot: &SessionSlot,
    messages: &[Message],
    turn_started: Option<SystemTime>,
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
            ))
            // The checkboxes are the pane state: CSS-only, so a collapsed
            // pane needs no script and a reload does not forget it while
            // the page is open. Checked is COLLAPSED.
            //
            // The stream is wired by attributes, as the home's is: htmx
            // opens it, swaps the `session` event's payload into the region,
            // and closes on the server's own `close` event. The listener
            // sits on a wrapper the payload never replaces, because htmx
            // re-processes what it swaps in and a listener on the region
            // itself would register one more per event.
            body hx-ext="sse, morph" sse-connect=(events_path(slot)) sse-close="close" {
                (icons::sprite())
                input type="checkbox" id="l" hidden;
                input type="checkbox" id="r" hidden;
                div #live sse-swap="session" hx-swap="morph:outerHTML" hx-target="#session-body" {
                    (columns(home, slot, messages, turn_started, roster, agents).await)
                }
                script src="/vendor/htmx.js" {}
                script src="/vendor/htmx-script.js" {}
                script src="/vendor/idiomorph.js" {}
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
    turn_started: Option<SystemTime>,
) -> Markup {
    let home = context(state, bound);
    let roster = state.surface.roster();
    let agents = state.surface.agents();
    columns(&home, slot, conversation, turn_started, &roster, &agents).await
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
    turn_started: Option<SystemTime>,
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

    html! {
    div #session-body {
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
                        span .facts { (account_chip(&accounts, chip.as_deref())) }
                    }
                    div .conv {
                        (chat_body(
                            waking,
                            row.and_then(|row| row.reason.as_deref()),
                            &units,
                            roster.cwd_for(slot).as_deref(),
                        ))
                        @if !waking {
                            (turn_row(messages, turn_started))
                        }
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

/// The page's own stream: a slot's region, one connection per tab.
fn events_path(slot: &SessionSlot) -> String {
    format!("{}/events", href(slot))
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

    html! {
        @if let Some(work) = &work {
            (git_section(work, diff.as_ref()))
        }
        @if !tasks.is_empty() {
            (tasks_section(&tasks))
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

/// What the chat column draws: the conversation, or the seat's own state
/// when there is none to draw.
///
/// It claims nothing about a spawn. This page cannot start a session, so a
/// line saying one is coming would be a promise no code keeps, and a seat
/// whose spawn failed would carry a failure mark above a line saying it is
/// connecting. A seat that is up with nothing said yet draws no skeleton
/// either: an empty conversation is empty.
fn chat_body(waking: bool, reason: Option<&str>, units: &[ChatUnit], cwd: Option<&Path>) -> Markup {
    if waking {
        return html! {
            div .hold .off {
                "not running"
                span .sub { (reason.unwrap_or("this seat has no session behind it")) }
            }
        };
    }
    conversation(units, cwd)
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
    let slot = slot.clone();
    let cwd = cwd.unwrap_or_default();
    tokio::task::spawn_blocking(move || surface.conversation(&slot, &cwd).messages)
        .await
        .unwrap_or_default()
}

/// The conversation, as the fold's units read: the user's own turns on their
/// own, and everything the assistant did in one work block after each.
fn conversation(units: &[ChatUnit], cwd: Option<&Path>) -> Markup {
    html! {
        @for turn in turns(units) {
            @match turn {
                Turn::Mine(text) => div .mine { (text) },
                Turn::Work(work) => div .work {
                    @for unit in work {
                        (unit_markup(unit, cwd))
                    }
                },
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
fn unit_markup(unit: &ChatUnit, cwd: Option<&Path>) -> Markup {
    match unit {
        ChatUnit::AssistantText { text } => html! { div .prose { (prose(text)) } },
        ChatUnit::ToolGroup { families, status } => tool_group(families, *status, cwd),
        ChatUnit::QuestionCard { asked } => question_card(asked),
        ChatUnit::PeerCard(card) => peer_card(card),
        ChatUnit::MessagingGroup { cards } => messaging_group(cards),
        ChatUnit::Notice(notice) => notice_row(notice),
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
    html! {
        details .kind open {
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
        details .leaf open[open] {
            summary {
                (status_icon(leaf.status))
                span .tn { (call_target(leaf, cwd)) }
                (icons::chevron(""))
            }
            @if !leaf.content.is_empty() {
                div .body { (leaf_body(&leaf.content)) }
            }
        }
    }
}

/// What a call's row opens on.
fn leaf_body(content: &[ToolCallContent]) -> Markup {
    html! {
        @for content in content {
            @match content {
                ToolCallContent::Diff { new_path, old, new, .. } => {
                    (diff_body(new_path, old, new))
                }
                ToolCallContent::Content { content } => (chunk_body(content)),
                ToolCallContent::McpResource { text, uri, .. } => {
                    div .term { (text.clone().unwrap_or_else(|| uri.clone())) }
                }
            }
        }
    }
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
            ToolFamily::Own("Edit" | "Write" | "MultiEdit" | "NotebookEdit") => "edit",
            ToolFamily::Config | ToolFamily::Worktree | ToolFamily::Tool | ToolFamily::Own(_) => {
                "tool"
            }
        },
    }
}

/// Whether a family's calls start open. A mutation's diff is what a reader
/// came for, and the mockup draws it without being asked.
fn opens_by_default(row: KindRow) -> bool {
    matches!(row, KindRow::Family(family) if matches!(family, ToolFamily::Own("Edit" | "Write" | "MultiEdit" | "NotebookEdit")))
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
    html! {
        details .msg open {
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

/// One settled turn's numbers, as the CLI wrote them.
struct TurnReport {
    duration_ms: u64,
    usage: Option<Usage>,
    cost: Option<f64>,
}

/// The turn row: what a turn did, under the work it did it with. A turn in
/// flight counts from when it started; a settled one reports what the CLI
/// wrote when it ended.
///
/// `duration_ms` is the turn's own wall clock. The API split is not drawn:
/// `duration_api_ms` on the wire is session-cumulative, not per-turn, so the
/// obvious subtraction reports negative local time on every turn after the
/// first. `num_turns` is not drawn either - it counts the agentic iterations
/// of one request, not the session's turns.
///
/// A usage block that is absent or all zero is the CLI attributing nothing
/// rather than measuring zero, so its numbers are left out instead of
/// claiming a turn that used no tokens.
fn turn_row(messages: &[Message], started: Option<SystemTime>) -> Markup {
    if let Some(started) = started {
        let elapsed = SystemTime::now().duration_since(started).unwrap_or_default();
        return html! {
            details .turninfo {
                summary {
                    span .ring {}
                    span { (elapsed_of(elapsed)) }
                    span .tog { "live" }
                }
            }
        };
    }
    let Some(report) = settled_turn(messages) else {
        return Markup::default();
    };
    let usage = report.usage.as_ref();
    let tokens = usage.map_or_else(Vec::new, turn_tokens);
    let cached = usage.map_or(0, cached_share);
    html! {
        details .turninfo {
            summary {
                span { "\u{21A9}" }
                span { (elapsed_of(Duration::from_millis(report.duration_ms))) }
                @for field in &tokens {
                    span .sep { "\u{b7}" }
                    span { (field) }
                }
                @if cached > 0 {
                    span .sep { "\u{b7}" }
                    span { (cached) "% cached" }
                }
                @if let Some(cost) = report.cost {
                    span .sep { "\u{b7}" }
                    span { (money(cost)) " cumulative" }
                }
                span .tog { "expand" }
            }
        }
    }
}

/// The last settled turn the conversation holds, and nothing for a
/// conversation that has not had one.
fn settled_turn(messages: &[Message]) -> Option<TurnReport> {
    messages.iter().rev().find_map(|msg| match msg {
        Message::Result { duration_ms, usage, total_cost_usd, .. } => {
            Some(TurnReport { duration_ms: *duration_ms, usage: *usage, cost: *total_cost_usd })
        }
        _ => None,
    })
}

/// The token counts a turn's usage block carries. Empty when the block is
/// all zero, which is the CLI attributing nothing rather than measuring.
fn turn_tokens(usage: &Usage) -> Vec<String> {
    let total = usage.input_tokens
        + usage.output_tokens
        + usage.cache_read_input_tokens
        + usage.cache_creation_input_tokens;
    if total == 0 {
        return Vec::new();
    }
    let mut out = vec![format!(
        "{}\u{2191} {}\u{2193}",
        compact(usage.input_tokens),
        compact(usage.output_tokens)
    )];
    if usage.cache_creation_input_tokens > 0 {
        out.push(format!("{} written", compact(usage.cache_creation_input_tokens)));
    }
    out
}

/// How much of a turn's input the prompt cache served, as a whole percent.
fn cached_share(usage: &Usage) -> u64 {
    let billed =
        usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
    (usage.cache_read_input_tokens * 100).checked_div(billed).unwrap_or(0)
}

/// A count as the mockup writes one: thousands to one decimal.
fn compact(count: u64) -> String {
    if count >= 1_000 {
        let tenths = count / 100;
        format!("{}.{}k", tenths / 10, tenths % 10)
    } else {
        count.to_string()
    }
}

/// A duration as the mockup writes one.
fn elapsed_of(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60),
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
