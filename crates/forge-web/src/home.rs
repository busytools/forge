//! The home page: every project, its agents, their states, and what needs
//! you.
//!
//! The fleet is gathered into [`HomeView`] before any markup runs, so the
//! markup is a pure function of data a test can build by hand.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use forge_primitives::tasks::{Task, TaskStatus};
use forge_primitives::{SessionLifecycleState, SessionSlot};
use forge_sessions::surface::{
    AccountsView, AgentRow, CliVersionInfo, DictateFailure, DictateModelState, DictateView,
    LoadingState, PendingKind, Roster, ViewSurface,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::brand;
use crate::server::root_block;
use crate::stream::Live;
use crate::unseen::Unseen;
use crate::work::{Gate, WorkCache, WorkState};

/// Everything the home draws with.
pub struct Home<'a> {
    pub surface: &'a ViewSurface,
    pub work: &'a WorkCache,
    /// What the stream has told the view.
    pub live: &'a Mutex<Live>,
    /// The address this page is served from.
    pub bound: SocketAddr,
    /// The mark and palette the config picked. `None` is the built-in.
    pub mark: Option<&'a str>,
    pub theme: Option<&'a str>,
}

/// The page's own view of the fleet.
pub struct HomeView {
    pub live_agents: usize,
    pub tasks: usize,
    pub projects: usize,
    /// The mark the config picked, `None` for the built-in. The region
    /// draws it, so it travels with the view rather than the request.
    pub mark: Option<String>,
    /// The claude versions the core holds: `None` until the first probe
    /// lands, and a snapshot whose sides are both empty when no probe has
    /// resolved one.
    pub cli: Option<CliVersionInfo>,
    pub band: Vec<Card>,
    pub orgs: Vec<OrgSection>,
}

/// One card in the system band. Quiet until it is not.
pub struct Card {
    pub title: &'static str,
    pub tone: Tone,
    pub value: String,
    pub detail: String,
}

/// How loud a card is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ready,
    Warn,
    Bad,
    Off,
}

impl Tone {
    /// The card's classes, quiet for the ready state.
    fn card(self) -> &'static str {
        match self {
            Self::Ready => "svc",
            Self::Warn => "svc warn",
            Self::Bad => "svc bad",
            Self::Off => "svc off",
        }
    }

    fn dot(self) -> &'static str {
        match self {
            Self::Ready => "ok",
            Self::Warn => "warn",
            Self::Bad => "bad",
            Self::Off => "off",
        }
    }
}

/// One org's projects, in the order `forge.toml` declares them.
pub struct OrgSection {
    pub name: String,
    /// How many of its projects have a session.
    pub live: usize,
    pub projects: Vec<ProjectRows>,
}

/// One project: its lead's row, its workers under it, or the dormant row a
/// project nobody has started gets.
pub struct ProjectRows {
    pub lead: Row,
    /// Empty when the project has no live session.
    pub workers: Vec<Row>,
    /// Why the project cannot start, when it cannot.
    pub refused: Option<&'static str>,
}

/// One row: the same shape for a lead and for a worker.
pub struct Row {
    pub state: State,
    pub name: String,
    /// The branch the agent's tree is on, and how much has moved in it.
    pub place: String,
    /// The task it holds, when it holds one.
    pub task: Option<TaskCell>,
    pub pending: Option<PendingKind>,
    pub reason: Option<String>,
    /// `None` for a project nobody has started.
    pub last_activity: Option<SystemTime>,
    /// What git said about the row's working tree, which is what the `what`
    /// cell says when there is no task and no ask to put there.
    pub gate: Gate,
}

/// The state a row draws: the core's lifecycle, plus the two states that
/// are not the core's to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Lifecycle(SessionLifecycleState),
    /// A turn finished on this session while the page was not showing it.
    Unseen,
    /// A project nothing has ever run in.
    NeverStarted,
}

/// A row's task, with the artifact it produced.
pub struct TaskCell {
    pub subject: String,
    pub chip: &'static str,
    pub artifact: Option<String>,
}

/// What a row starts from, before its working tree is read.
struct Seed<'a> {
    slot: &'a SessionSlot,
    name: String,
    state: State,
    pending: Option<PendingKind>,
    reason: Option<String>,
    task: Option<&'a Task>,
    last_activity: Option<SystemTime>,
}

impl Seed<'_> {
    fn from_agent<'a>(agent: &'a AgentRow, task: Option<&'a Task>, unseen: &Unseen) -> Seed<'a> {
        Seed {
            slot: &agent.slot,
            name: agent.label.clone(),
            state: state_of_agent(agent, unseen),
            pending: agent.pending,
            reason: agent.reason.clone(),
            task,
            last_activity: agent.last_activity,
        }
    }
}

/// What an agent's row draws. Two promotions the core does not make, both
/// about what the mark means rather than what the session is:
///
/// - anything moving means work is happening, and a backgrounded task is
///   work even after the turn that started it settled;
/// - a turn that finished while this view was not showing the session is
///   the one state that answers "what changed while I was away".
fn state_of_agent(agent: &AgentRow, unseen: &Unseen) -> State {
    match agent.lifecycle {
        SessionLifecycleState::Idle if agent.has_background_work => {
            State::Lifecycle(SessionLifecycleState::Running)
        }
        SessionLifecycleState::Idle if unseen.is_unseen(&agent.slot) => State::Unseen,
        lifecycle => State::Lifecycle(lifecycle),
    }
}

/// Gather the fleet and render it.
pub async fn render(home: &Home<'_>) -> Markup {
    page(&view_of(home).await, home.theme)
}

/// Gather the fleet and render the region the stream swaps in.
pub async fn render_region(home: &Home<'_>) -> Markup {
    region(&view_of(home).await)
}

/// Read the core into the shape the markup wants, with each row's working
/// tree out of the cache.
async fn view_of(home: &Home<'_>) -> HomeView {
    let live = Live::lock(home.live).snapshot();
    let roster = home.surface.roster();
    let agents = home.surface.agents();
    let accounts = home.surface.accounts();
    let dictate = home.surface.dictate();

    let mut orgs: Vec<OrgSection> = Vec::new();
    for project in &roster.projects {
        let rows = agents.for_project(&project.key);
        let tasks = roster.tasks_for_project(&project.name);
        let lead_task = task_for(&tasks, "lead");
        // A project with no live session is asleep if anything ever ran in
        // it, and never-started if nothing has: the mock draws the two
        // differently, and a restart puts every project in the first case,
        // so reading the lifecycle alone would call the whole fleet new.
        let last_ran = project.sessions.iter().filter_map(|view| view.last_activity).max();
        let dormant = Seed {
            slot: &SessionSlot::lead(&project.org, &project.name),
            name: project.name.clone(),
            state: if last_ran.is_some() {
                State::Lifecycle(SessionLifecycleState::Sleeping)
            } else {
                State::NeverStarted
            },
            pending: None,
            reason: None,
            task: None,
            last_activity: last_ran,
        };

        let (lead, workers) = match rows.split_first() {
            Some((head, rest)) => {
                // A project's row is its lead, and the home names it for
                // the project: the lead's label is its identity, not what
                // the row is called here.
                let mut seed = Seed::from_agent(head, lead_task, &live.unseen);
                seed.name.clone_from(&project.name);
                (
                    row_for(home, &roster, seed).await,
                    worker_rows(home, &roster, rest, &tasks, &live.unseen).await,
                )
            }
            None => (row_for(home, &roster, dormant).await, Vec::new()),
        };
        let refused = rows
            .is_empty()
            .then(|| refusal(project.has_model, roster.would_bind(&project.key)))
            .flatten();

        push_org(
            &mut orgs,
            project.org.clone(),
            usize::from(!rows.is_empty()),
            ProjectRows { lead, workers, refused },
        );
    }

    // The orgs read alphabetically rather than in whatever order
    // forge.toml declares them; the projects inside each keep their
    // declared order.
    orgs.sort_by(|a, b| a.name.cmp(&b.name));

    HomeView {
        live_agents: agents.all().len(),
        tasks: roster
            .projects
            .iter()
            .map(|project| roster.tasks_for_project(&project.name).len())
            .sum(),
        projects: roster.projects.len(),
        mark: home.mark.map(str::to_owned),
        cli: home.surface.cli_version(),
        band: band(home, &accounts, &dictate),
        orgs,
    }
}

async fn worker_rows(
    home: &Home<'_>,
    roster: &Roster,
    rest: &[AgentRow],
    tasks: &[Task],
    unseen: &Unseen,
) -> Vec<Row> {
    let mut workers = Vec::new();
    for agent in rest {
        let task = task_for(tasks, &agent.label);
        workers.push(row_for(home, roster, Seed::from_agent(agent, task, unseen)).await);
    }
    workers
}

/// Whether `task` is held by the session labelled `label`. The owner is a
/// slot, and the row it belongs on is the one carrying that label.
fn owned_by(task: &Task, label: &str) -> bool {
    task.owner.as_ref().is_some_and(|owner| owner.label() == label)
}

/// What the artifact column shows: one short token, not the whole thing.
/// A task's artifact is a PR URL or a path, and either would push the
/// row's other cells off a narrow screen, so a PR URL reads `PR 148` and
/// a path reads its file name.
fn artifact_label(artifact: &str) -> String {
    let trimmed = artifact.trim_end_matches('/');
    let mut parts = trimmed.rsplit('/');
    let last = parts.next().unwrap_or("");
    let kind = parts.next().unwrap_or("");
    if !last.is_empty() && (kind == "pull" || kind == "issues") {
        let prefix = if kind == "pull" { "PR " } else { "#" };
        return format!("{prefix}{last}");
    }
    if last.is_empty() { trimmed.to_owned() } else { last.to_owned() }
}

/// The task a row shows: the one this label holds that is furthest from
/// done, in-progress first.
///
/// Picking by position instead would show whatever the store happened to
/// return first, which is insertion order within a run and key order across
/// a restart: a label reused by a new worker can hold the last occupant's
/// finished task, and the row would show it under a state column saying
/// running. The TUI sorts in-progress first for the same reason.
fn task_for<'a>(tasks: &'a [Task], label: &str) -> Option<&'a Task> {
    tasks.iter().filter(|task| owned_by(task, label)).min_by_key(|task| match task.status {
        TaskStatus::InProgress => 0,
        TaskStatus::Blocked => 1,
        TaskStatus::Pending => 2,
        TaskStatus::Completed => 3,
    })
}

/// The four cards. Each is quiet until its own state says otherwise.
fn band(home: &Home<'_>, accounts: &AccountsView, dictate: &DictateView) -> Vec<Card> {
    let gateway = match (&accounts.gateway.bind_error, accounts.gateway.ready) {
        (Some(error), _) => Card {
            title: "gateway",
            tone: Tone::Bad,
            value: format!("failed :{}", accounts.gateway.port),
            detail: error.clone(),
        },
        (None, true) => Card {
            title: "gateway",
            tone: Tone::Ready,
            value: format!("bound :{}", accounts.gateway.port),
            detail: "inference listener".to_owned(),
        },
        (None, false) => Card {
            title: "gateway",
            tone: Tone::Warn,
            value: "binding".to_owned(),
            detail: "inference listener".to_owned(),
        },
    };

    let models = &dictate.snapshot.models;
    let dictation = match (models.is_empty(), &dictate.snapshot.failure) {
        (true, _) => Card {
            title: "dictation",
            tone: Tone::Off,
            value: "off".to_owned(),
            detail: "enabled = false".to_owned(),
        },
        (false, Some(failure)) => Card {
            title: "dictation",
            tone: Tone::Bad,
            value: failure_kind(failure).to_owned(),
            detail: failure_file(failure),
        },
        (false, None) => {
            let ready =
                models.iter().filter(|model| model.state == DictateModelState::Ready).count();
            Card {
                title: "dictation",
                tone: if ready == models.len() { Tone::Ready } else { Tone::Warn },
                value: format!("{ready} of {} loaded", models.len()),
                detail: models
                    .iter()
                    .map(|model| model_state(&model.state))
                    .collect::<Vec<_>>()
                    .join(", "),
            }
        }
    };

    let ready = accounts.loading.iter().filter(|row| row.state == LoadingState::Ready).count();
    let bailed = accounts.loading.iter().filter(|row| row.state == LoadingState::Bailed).count();
    let accounts_card = Card {
        title: "accounts",
        tone: match (bailed, accounts.all_loaded) {
            (1.., _) => Tone::Bad,
            (_, false) => Tone::Warn,
            (_, true) => Tone::Ready,
        },
        value: if bailed == 0 {
            format!("{ready} ready")
        } else {
            format!("{ready} ready \u{b7} {bailed} bailed")
        },
        detail: if accounts.all_loaded { "probed" } else { "probing" }.to_owned(),
    };

    vec![
        gateway,
        Card {
            title: "web",
            tone: Tone::Ready,
            value: home.bound.to_string(),
            detail: "this page".to_owned(),
        },
        dictation,
        accounts_card,
    ]
}

fn failure_kind(failure: &DictateFailure) -> &'static str {
    match failure {
        DictateFailure::HashMismatch { .. } => "hash mismatch",
        _ => "failed",
    }
}

fn failure_file(failure: &DictateFailure) -> String {
    match failure {
        DictateFailure::HashMismatch { path, .. } => path
            .file_name()
            .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned()),
        DictateFailure::Cancelled { .. } => "stopped".to_owned(),
        DictateFailure::Other { message } => message.clone(),
    }
}

fn model_state(state: &DictateModelState) -> &'static str {
    match state {
        DictateModelState::Pending => "waiting",
        DictateModelState::Downloading { .. } => "fetching",
        DictateModelState::Verifying => "verifying",
        DictateModelState::Fetched => "fetched",
        DictateModelState::Loading => "loading",
        DictateModelState::Ready => "loaded",
        DictateModelState::Failed(_) => "failed",
    }
}

/// Why a spawn here would be refused, or `None` when it would not be: the
/// two the launchpad already tells apart, from the same two reads.
fn refusal(has_model: bool, would_bind: bool) -> Option<&'static str> {
    match (has_model, would_bind) {
        (false, _) => Some("no model declared - add `model` to this project"),
        (true, false) => Some("no usable accounts"),
        (true, true) => None,
    }
}

/// One row, with its working tree out of the cache. The roster is passed
/// in rather than collected here: it walks the project catalog, and a walk
/// per row is the per-row-loop trap the view surface's own notes name.
async fn row_for(home: &Home<'_>, roster: &Roster, seed: Seed<'_>) -> Row {
    let work = match roster.cwd_for(seed.slot) {
        Some(cwd) => Some(home.work.snapshot(seed.slot, cwd.as_path()).await),
        None => None,
    };
    Row {
        state: seed.state,
        name: seed.name,
        place: place_of(work.as_ref()),
        gate: work.as_ref().map_or(Gate::InRepo, |work| work.gate),
        task: seed.task.map(|task| TaskCell {
            subject: task.subject.clone(),
            chip: chip_for(task.status),
            artifact: task.artifact.clone(),
        }),
        pending: seed.pending,
        reason: seed.reason,
        last_activity: seed.last_activity,
    }
}

/// The `where` cell: the branch the tree is on, and what has moved in it.
/// Empty when the directory is not a repository, or is not there.
fn place_of(work: Option<&WorkState>) -> String {
    let Some(work) = work else {
        return String::new();
    };
    let files = match work.changed {
        Some(0) | None => String::new(),
        Some(1) => "1 file".to_owned(),
        Some(count) => format!("{count} files"),
    };
    match (work.branch.as_deref(), files.is_empty()) {
        (None, true) => String::new(),
        (None, false) => files,
        (Some(branch), true) => branch.to_owned(),
        (Some(branch), false) => format!("{branch} \u{b7} {files}"),
    }
}

fn chip_for(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Completed => "done",
    }
}

fn push_org(orgs: &mut Vec<OrgSection>, name: String, live: usize, rows: ProjectRows) {
    match orgs.iter_mut().find(|section| section.name == name) {
        Some(section) => {
            section.live += live;
            section.projects.push(rows);
        }
        None => orgs.push(OrgSection { name, live, projects: vec![rows] }),
    }
}

/// The page. Pure: everything it draws comes from `view`.
fn page(view: &HomeView, theme_name: Option<&str>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "forge \u{b7} home" }
                (root_block(theme_name))
                link rel="stylesheet" href="/home.css";
                link rel="icon" href="/favicon.svg" type="image/svg+xml";
            }
            // The stream, wired by attributes: htmx opens it, swaps the
            // `fleet` event's payload into the region, and closes on the
            // server's own `close` event rather than reconnecting to a
            // stream that has ended. Nothing on the page is script of
            // forge's own.
            //
            // Both extensions are named, and `morph` is not optional: an
            // undeclared swap style is not an error, it falls back to
            // filling the target, which nests the region inside itself.
            body hx-ext="sse, morph" sse-connect="/events" sse-close="close" {
                // The swap lives on a wrapper the payload never replaces.
                // htmx re-processes whatever it swaps in, so a `sse-swap`
                // on the region itself registers one more listener for
                // every event - measured at ninety swaps per update and
                // climbing, which is a page that cooks a core by itself.
                div #fleet sse-swap="fleet" hx-swap="morph:outerHTML" hx-target="#home" {
                    (region(view))
                }
                script src="/vendor/htmx.js" {}
                script src="/vendor/htmx-sse.js" {}
                script src="/vendor/idiomorph.js" {}
            }
        }
    }
}

/// The region the stream swaps in: everything the page draws from the
/// core, and nothing it draws from the request. The payload is this
/// element itself, and it carries no wiring of its own - the listener that
/// swaps it lives on the wrapper outside it.
fn region(view: &HomeView) -> Markup {
    html! {
        div .wrap #home {
            header .top {
                div .brand {
                    span .mark { (PreEscaped(brand::mark_svg(view.mark.as_deref()))) }
                    span .word { "forge" }
                }
                div .versions {
                    b { "v" (env!("CARGO_PKG_VERSION")) }
                    @if let Some(installed) = view.cli.as_ref().and_then(|cli| cli.installed.as_deref()) {
                        " \u{b7} claude " (installed)
                    }
                    @if let Some(latest) = available(view.cli.as_ref()) {
                        " \u{b7} " span .upd { "\u{2191} v" (latest) " available" }
                    }
                }
                div .totals {
                    span .n { (view.live_agents) } " agents \u{b7} "
                    span .n { (view.tasks) } " tasks \u{b7} "
                    (view.projects) " projects"
                }
            }
            section .band {
                @for card in &view.band {
                    div class=(card.tone.card()) {
                        div .k { span .dot .(card.tone.dot()) {} (card.title) }
                        div .v { (card.value) }
                        div .sub { (card.detail) }
                    }
                }
            }
            @if view.orgs.is_empty() {
                (empty_state(view.mark.as_deref()))
            }
            @for org in &view.orgs {
                section .org {
                    h2 {
                        (org.name) span .rule {}
                        span .counts { (counts_of(org)) }
                    }
                    ul .list {
                        @for project in &org.projects {
                            li .node {
                                (row(&project.lead, project.refused))
                                @if !project.workers.is_empty() {
                                    ul .children {
                                        @for worker in &project.workers {
                                            li .node { (row(worker, None)) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The published version to name, when npm has a newer one than the
/// installed CLI. `has_update` is the rule and already requires both sides.
fn available(cli: Option<&CliVersionInfo>) -> Option<&str> {
    let cli = cli?;
    if cli.has_update() { cli.latest.as_deref() } else { None }
}

fn counts_of(org: &OrgSection) -> String {
    let asleep = org.projects.len() - org.live;
    match (org.live, asleep) {
        (0, _) => format!("{asleep} asleep"),
        (_, 0) => format!("{} live", org.live),
        _ => format!("{} live \u{b7} {asleep} asleep", org.live),
    }
}

fn empty_state(mark: Option<&str>) -> Markup {
    html! {
        div .empty {
            span .mark { (PreEscaped(brand::mark_svg(mark))) }
            p .t { "No projects yet" }
            p .d {
                "Projects come from " code { "forge.toml" } ". Add an "
                code { "[[orgs.projects]]" }
                " entry pointing at a repository, and it appears here."
            }
        }
    }
}

/// One row. The dot's shape carries the state, so colour is never the only
/// signal, and a lead and a worker share every column.
fn row(row: &Row, refused: Option<&'static str>) -> Markup {
    let mark = state_of(row.state);
    html! {
        div .row .(mark.class) {
            span .dot .(mark.dot) {}
            span .name { (&row.name) }
            span .where { (&row.place) }
            span .what {
                @if let Some(pending) = row.pending {
                    span .txt { (waiting_on(pending)) }
                } @else if let Some(task) = &row.task {
                    span .txt { (&task.subject) }
                    span .st { (task.chip) }
                    @if let Some(artifact) = &task.artifact {
                        a href=(artifact) target="_blank" rel="noreferrer" { (artifact_label(artifact)) }
                    }
                } @else if let Some(refused) = refused {
                    span .txt { (refused) }
                } @else if let Some(line) = gate_line(row.gate) {
                    span .txt { (line) }
                } @else {
                    span .txt { "\u{b7}" }
                }
            }
            span .when { (when_of(row.state, row.last_activity)) }
        }
        @if let Some(reason) = &row.reason {
            div .note { (reason) }
        }
    }
}

/// What a row says when its working directory is not there to read. `InRepo`
/// is the row that has nothing to explain, and says nothing.
fn gate_line(gate: Gate) -> Option<&'static str> {
    match gate {
        Gate::InRepo => None,
        Gate::NotARepository => Some("not a git repository, so there is no branch to show"),
        Gate::Gone => Some("its working directory is not there"),
        Gate::ScannerFailed => Some("its working tree could not be read"),
    }
}

/// What a held session is waiting on a person for.
fn waiting_on(pending: PendingKind) -> &'static str {
    match pending {
        PendingKind::Question => "asked you a question",
        PendingKind::Permission => "a permission prompt is waiting",
    }
}

/// A row's own class and dot: one shape per meaning.
struct Mark {
    class: &'static str,
    dot: &'static str,
}

fn state_of(state: State) -> Mark {
    match state {
        State::Lifecycle(SessionLifecycleState::Running) => Mark { class: "running", dot: "live" },
        State::Lifecycle(SessionLifecycleState::Spawning) => {
            Mark { class: "spawning", dot: "live" }
        }
        State::Lifecycle(SessionLifecycleState::Idle) => Mark { class: "idle", dot: "live" },
        // Finished, and this view has not looked at it: the one state that
        // answers "what changed while I was away", and a third shape so it
        // is findable without colour.
        State::Unseen => Mark { class: "unseen", dot: "ok" },
        State::Lifecycle(SessionLifecycleState::Attention) => Mark { class: "needs", dot: "warn" },
        State::Lifecycle(SessionLifecycleState::AuthRequired) => Mark { class: "auth", dot: "bad" },
        State::Lifecycle(SessionLifecycleState::Failed) => Mark { class: "failed", dot: "bad" },
        // Both are a session that is not there: the subprocess is gone, or
        // `/logout` took it. One mark, because the row says the same thing.
        State::Lifecycle(SessionLifecycleState::Sleeping | SessionLifecycleState::LoggedOut) => {
            Mark { class: "asleep", dot: "off" }
        }
        State::NeverStarted => Mark { class: "never", dot: "off" },
    }
}

/// How long ago the session last wrote. A project nothing has run in is
/// `never`; a live session with no transcript yet has only just started,
/// which is `now` rather than an absence.
fn when_of(state: State, last_activity: Option<SystemTime>) -> String {
    if state == State::NeverStarted {
        return "never".to_owned();
    }
    let Some(at) = last_activity else {
        return "now".to_owned();
    };
    let elapsed = SystemTime::now().duration_since(at).unwrap_or(Duration::ZERO);
    match elapsed.as_secs() {
        0..60 => "now".to_owned(),
        seconds if seconds < 3600 => format!("{}m", seconds / 60),
        seconds if seconds < 86_400 => format!("{}h", seconds / 3600),
        seconds => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_of(state: State) -> Row {
        Row {
            state,
            name: "forge".to_owned(),
            place: String::new(),
            task: None,
            pending: None,
            reason: None,
            last_activity: None,
            gate: Gate::InRepo,
        }
    }

    fn work(branch: Option<&str>, changed: Option<usize>) -> WorkState {
        WorkState { branch: branch.map(str::to_owned), changed, gate: Gate::InRepo }
    }

    fn render(view: &HomeView) -> String {
        page(view, None).into_string()
    }

    fn empty() -> HomeView {
        HomeView {
            live_agents: 0,
            tasks: 0,
            projects: 0,
            mark: None,
            cli: None,
            band: Vec::new(),
            orgs: Vec::new(),
        }
    }

    /// The header names the versions the core holds, the same answer the
    /// TUI's boot line draws: what is installed, and what npm publishes
    /// when that is newer.
    #[test]
    fn the_header_names_the_installed_version_and_any_update() {
        let mut view = empty();
        view.cli = Some(CliVersionInfo {
            installed: Some("2.1.156".to_owned()),
            latest: Some("2.1.201".to_owned()),
        });

        let markup = render(&view);

        assert!(
            markup.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "the forge version still leads the line: {markup}",
        );
        assert!(markup.contains("claude 2.1.156"), "the installed version draws: {markup}");
        assert!(
            markup.contains("\u{2191} v2.1.201 available"),
            "and the available one when npm is ahead: {markup}",
        );
        assert!(
            markup.contains("class=\"upd\""),
            "the available version is dressed as an update rather than plain text: {markup}",
        );
    }

    /// A side the core has not resolved draws nothing rather than a
    /// placeholder. The page has no fixed row to keep one for, unlike the
    /// TUI's panel.
    #[test]
    fn a_version_the_core_has_not_resolved_draws_nothing() {
        let mut view = empty();
        view.cli = Some(CliVersionInfo { installed: Some("2.1.156".to_owned()), latest: None });
        let markup = render(&view);
        assert!(
            markup.contains("claude 2.1.156"),
            "the installed version draws whether or not npm has anything newer: {markup}",
        );
        assert!(
            !markup.contains("\u{2191}"),
            "no arrow without a newer published version: {markup}",
        );

        view.cli = None;
        let markup = render(&view);
        assert!(
            markup.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "the forge version draws alone: {markup}",
        );
        assert!(
            !markup.contains("claude"),
            "and nothing stands in for a version the core has not read: {markup}",
        );
    }

    /// Catches a state whose mark is borrowed from its neighbour: a
    /// sleeping session drawn like a live one, or a failure without its own
    /// shape.
    #[test]
    fn every_state_has_its_own_mark() {
        let marks = [
            (State::Lifecycle(SessionLifecycleState::Running), "running"),
            (State::Lifecycle(SessionLifecycleState::Spawning), "spawning"),
            (State::Lifecycle(SessionLifecycleState::Idle), "idle"),
            (State::Lifecycle(SessionLifecycleState::Attention), "needs"),
            (State::Lifecycle(SessionLifecycleState::AuthRequired), "auth"),
            (State::Lifecycle(SessionLifecycleState::Failed), "failed"),
            (State::Lifecycle(SessionLifecycleState::Sleeping), "asleep"),
            (State::Lifecycle(SessionLifecycleState::LoggedOut), "asleep"),
            (State::NeverStarted, "never"),
        ];
        for (state, class) in marks {
            assert_eq!(state_of(state).class, class, "{class} draws its own class");
        }
    }

    /// The `where` cell shows the branch and what has moved, and nothing at
    /// all when there is no repository to read.
    #[test]
    fn the_work_column_says_what_moved() {
        assert_eq!(place_of(None), "", "no repository is an empty column");
        assert_eq!(
            place_of(Some(&work(None, None))),
            "",
            "a directory outside a repository has no branch to show",
        );
        assert_eq!(
            place_of(Some(&work(Some("main"), Some(0)))),
            "main",
            "a clean tree is the branch"
        );
        assert_eq!(
            place_of(Some(&work(Some("main"), Some(1)))),
            "main \u{b7} 1 file",
            "one file is not one files",
        );
        assert_eq!(place_of(Some(&work(Some("main"), Some(7)))), "main \u{b7} 7 files");
    }

    /// Catches a row showing a finished task while its state column says
    /// running: a label can hold more than one task across a restart, and
    /// the one the store returns first is not the one being worked on.
    #[test]
    fn a_row_shows_the_task_still_being_worked_on() {
        let task = |id: &str, status: TaskStatus| Task {
            id: id.into(),
            project_name: "forge".to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status,
            owner: Some(SessionSlot::lead("Org", "forge")),
            parent: None,
            artifact: None,
            estimate: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        };
        // The finished one first, which is what the store tends to return.
        let tasks =
            vec![task("done", TaskStatus::Completed), task("running", TaskStatus::InProgress)];

        assert_eq!(
            task_for(&tasks, "lead").map(|task| task.id.as_str()),
            Some("running"),
            "the task still being worked on is the one the row shows",
        );
        assert_eq!(
            task_for(&tasks, "somebody-else"),
            None,
            "and a label holding none shows none, rather than its neighbour's",
        );
    }

    /// A row whose tree git could not read says which of the two it was.
    /// Catches a row that falls through to the taskless middot, which is
    /// what an unreadable tree used to look like.
    #[test]
    fn an_unreadable_tree_says_why() {
        assert_eq!(gate_line(Gate::InRepo), None, "a row with a repository has nothing to explain");
        assert_eq!(
            gate_line(Gate::NotARepository),
            Some("not a git repository, so there is no branch to show"),
        );
        assert_eq!(
            gate_line(Gate::Gone),
            Some("its working directory is not there"),
            "a missing path must not be reported as a project without a repository, and the \
             line cannot claim a worker: a project's own directory goes missing the same way",
        );
        assert_eq!(
            gate_line(Gate::ScannerFailed),
            Some("its working tree could not be read"),
            "a git that would not run is forge's problem, not the project's",
        );

        let mut row = row_of(State::Lifecycle(SessionLifecycleState::Idle));
        row.gate = Gate::NotARepository;
        assert!(
            row_markup(&row).contains("not a git repository"),
            "the row says it: {}",
            row_markup(&row),
        );
    }

    /// The page draws one header per org, the fleet count in the header
    /// line, and a project's workers under its lead.
    #[test]
    fn an_artifact_reads_as_one_short_token() {
        assert_eq!(
            artifact_label("https://github.com/GraniteProtocol/granite-backend/pull/148"),
            "PR 148",
            "a pull request reads as its number, not as the URL",
        );
        assert_eq!(
            artifact_label("https://github.com/o/r/issues/9/"),
            "#9",
            "a trailing slash does not become the label",
        );
        assert_eq!(
            artifact_label("docs/plans/2026-09-26-web-home.md"),
            "2026-09-26-web-home.md",
            "a path reads as its file name",
        );
        assert_eq!(artifact_label("main"), "main", "and a bare token is itself");
    }

    #[test]
    fn the_page_groups_projects_under_their_org() {
        let view = HomeView {
            live_agents: 2,
            tasks: 1,
            projects: 3,
            mark: None,
            cli: None,
            band: Vec::new(),
            orgs: vec![
                OrgSection {
                    name: "Busytools".to_owned(),
                    live: 1,
                    projects: vec![ProjectRows {
                        lead: row_of(State::Lifecycle(SessionLifecycleState::Idle)),
                        workers: vec![Row {
                            name: "em-dash-sweep".to_owned(),
                            ..row_of(State::Lifecycle(SessionLifecycleState::Running))
                        }],
                        refused: None,
                    }],
                },
                OrgSection {
                    name: "Personal".to_owned(),
                    live: 0,
                    projects: vec![ProjectRows {
                        lead: row_of(State::NeverStarted),
                        workers: Vec::new(),
                        refused: Some("no model declared - add `model` to this project"),
                    }],
                },
            ],
        };

        let markup = render(&view);

        assert!(markup.contains(">Busytools<"), "one header per org: {markup}");
        assert!(markup.contains(">Personal<"));
        assert!(
            markup.contains("<span class=\"n\">2</span> agents"),
            "the header line carries the fleet count: {markup}",
        );
        assert!(markup.contains("3 projects"), "and the project count: {markup}");
        assert_eq!(
            markup.matches("class=\"children\"").count(),
            1,
            "only the project with workers nests them",
        );
        assert!(markup.contains("1 live"), "an org counts its own: {markup}");
        assert!(markup.contains("1 asleep"), "and its dormant ones: {markup}");
        assert!(
            markup.contains("no model declared"),
            "a refused project says which refusal: {markup}",
        );
    }

    /// A fleet with nothing in it is the empty state, not a blank page.
    #[test]
    fn an_empty_fleet_draws_the_empty_state() {
        let markup = render(&empty());

        assert!(markup.contains("No projects yet"), "a fresh install gets a page: {markup}");
        assert!(markup.contains("[[orgs.projects]]"), "and a way out of it: {markup}");
    }

    /// Catches a relative time that never rolls over, and one that answers
    /// a live session with "never".
    #[test]
    fn the_when_column_rolls_over() {
        let now = SystemTime::now();
        let live = State::Lifecycle(SessionLifecycleState::Idle);
        assert_eq!(when_of(State::NeverStarted, None), "never");
        assert_eq!(
            when_of(live, None),
            "now",
            "a live session with no transcript yet has only just started",
        );
        assert_eq!(when_of(live, Some(now)), "now");
        assert_eq!(when_of(live, Some(now - Duration::from_secs(17 * 60))), "17m");
        assert_eq!(when_of(live, Some(now - Duration::from_secs(8 * 3600))), "8h");
        assert_eq!(when_of(live, Some(now - Duration::from_secs(10 * 86_400))), "10d");
    }

    /// A project that cannot start carries why, since the row cannot look
    /// clickable and has to say something instead.
    #[test]
    fn a_refused_project_says_which_refusal() {
        assert_eq!(refusal(false, true), Some("no model declared - add `model` to this project"));
        assert_eq!(refusal(true, false), Some("no usable accounts"));
        assert_eq!(refusal(true, true), None, "a project that can start carries no refusal");
    }

    fn agent(lifecycle: SessionLifecycleState, background: bool) -> AgentRow {
        AgentRow {
            slot: SessionSlot::lead("Org", "forge"),
            label: "lead".to_owned(),
            lifecycle,
            has_background_work: background,
            pending: None,
            last_activity: None,
            reason: None,
        }
    }

    /// The two promotions the core does not make, because both are about
    /// what a mark means rather than what the session is. Catches a row
    /// that reads the lifecycle alone.
    #[test]
    fn a_row_carries_the_two_states_the_core_cannot_know() {
        let unseen = Unseen::new();

        assert_eq!(
            state_of_agent(&agent(SessionLifecycleState::Idle, true), &unseen),
            State::Lifecycle(SessionLifecycleState::Running),
            "a backgrounded task is work still happening, so the row moves",
        );
        assert_eq!(
            state_of_agent(&agent(SessionLifecycleState::Running, true), &unseen),
            State::Lifecycle(SessionLifecycleState::Running),
            "a turn in flight moves whatever else is true",
        );

        let mut unseen = Unseen::new();
        unseen.mark_completed(&SessionSlot::lead("Org", "forge"));
        assert_eq!(
            state_of_agent(&agent(SessionLifecycleState::Idle, false), &unseen),
            State::Unseen,
            "a turn that finished unlooked-at is the diamond",
        );
        assert_eq!(
            state_of_agent(&agent(SessionLifecycleState::Attention, false), &unseen),
            State::Lifecycle(SessionLifecycleState::Attention),
            "a session that needs you outranks the diamond it also earned",
        );
    }

    /// Catches a page whose stream is not wired at all, wired to fill the
    /// region rather than replace it, or wired on the region itself. All
    /// three pass the wire tests, which read bytes rather than load them.
    #[test]
    fn the_page_opens_the_stream_by_attribute() {
        let markup = render(&empty());

        assert!(
            markup.contains("hx-ext=\"sse, morph\" sse-connect=\"/events\" sse-close=\"close\""),
            "the body opens the stream, enables the swap, and closes on the server's own \
             event: {markup}",
        );
        assert!(
            markup.contains(
                "id=\"fleet\" sse-swap=\"fleet\" hx-swap=\"morph:outerHTML\" hx-target=\"#home\""
            ),
            "the listener sits on a wrapper outside the payload: {markup}",
        );
        assert!(
            !markup.contains("id=\"home\" sse-swap"),
            "and not on the region, which htmx would re-process into another listener per \
             event: {markup}",
        );
        for asset in ["/vendor/htmx.js", "/vendor/htmx-sse.js", "/vendor/idiomorph.js"] {
            assert!(markup.contains(asset), "the page loads {asset}: {markup}");
        }
    }

    /// Catches a swap payload that is the whole document: assigning that
    /// into the region nests a second `.wrap` and a second `#home` on the
    /// first event, and re-parses the head inside the body.
    #[test]
    fn the_stream_payload_is_the_region_and_not_the_document() {
        let view = empty();

        let payload = region(&view).into_string();

        assert!(
            payload.contains("class=\"wrap\" id=\"home\""),
            "the region is the wrap: {payload}"
        );
        for outside in ["<!DOCTYPE", "<html", "<head>", "<body>", "<title>"] {
            assert!(
                !payload.contains(outside),
                "and nothing outside it, found {outside}: {payload}"
            );
        }
    }

    /// A held session's row says what it is waiting on, so needs-you is
    /// actionable from the home rather than only visible.
    #[test]
    fn a_held_row_names_its_ask() {
        let mut row = row_of(State::Lifecycle(SessionLifecycleState::Attention));
        row.pending = Some(PendingKind::Question);
        assert!(row_markup(&row).contains("asked you a question"));

        row.pending = Some(PendingKind::Permission);
        assert!(row_markup(&row).contains("a permission prompt is waiting"));
    }

    /// A failed row carries the reason the core recorded, under the row.
    #[test]
    fn a_failed_row_carries_its_reason() {
        let mut row = row_of(State::Lifecycle(SessionLifecycleState::Failed));
        row.reason = Some("OAuth token expired; run /login to retry".to_owned());

        let markup = row_markup(&row);

        assert!(
            markup.contains("OAuth token expired; run /login to retry"),
            "the reason is on the row: {markup}",
        );
        assert!(markup.contains("class=\"note\""), "in the mock's own note line: {markup}");
    }

    fn row_markup(row: &Row) -> String {
        super::row(row, None).into_string()
    }
}
