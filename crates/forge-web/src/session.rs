//! The session page: the projects rail, the conversation, and the
//! inspector.
//!
//! Three columns over one sheet. The rail and the inspector collapse to
//! nothing and their handles live in the chat header, so a collapsed pane
//! leaves no edge behind and its control stays reachable. Every column
//! reads the core through the view surface: this module holds no state of
//! its own.

use std::net::SocketAddr;

use forge_primitives::SessionLifecycleState;
use forge_primitives::SessionSlot;
use forge_primitives::account::AccountAuth;
use forge_sessions::surface::{AccountsView, LoadingState, PendingKind, Roster};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::home::{Home, Row, Seed, State};
use crate::server::{WebState, root_block};
use crate::stream::Live;

/// The handle that brings the projects rail back, and the one that brings
/// the inspector back. Both live with the title so they stay clickable
/// while their pane is gone.
const RAIL_TOGGLE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M3 6h18M3 12h18M3 18h18"/></svg>"#;
const INSPECTOR_TOGGLE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M15 4v16"/></svg>"#;

/// What the route found for a slot.
pub enum Found {
    /// The seat is in the roster and something is running behind it.
    Open(Markup),
    /// The seat is in the roster; nothing is behind it yet, so the page
    /// waits rather than refusing.
    Waking(Markup),
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
    let Some(seat) = roster.projects.iter().find(|seat| seat.org == org && seat.name == project)
    else {
        return Found::Absent;
    };
    // A project declares one seat of its own that exists whether or not it
    // has ever run; a worker is a seat only while the roster can name it.
    let slot = if label == "lead" {
        SessionSlot::lead(org, project)
    } else {
        let named = surface.agents().for_project(&seat.key).iter().any(|row| row.label == label)
            || surface.workers().for_project(&seat.key).iter().any(|row| row.label == label);
        if !named {
            return Found::Absent;
        }
        SessionSlot::worker(org, project, label)
    };
    if roster.has_agent(&slot) {
        Found::Open(shell(&context(state, bound), &slot).await)
    } else {
        Found::Waking(shell(&context(state, bound), &slot).await)
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
    }
}

/// The page. One page for both outcomes: the columns are as real for a
/// seat nothing is running behind as for one that is up, and only the chat
/// column says which of the two it is looking at.
async fn shell(home: &Home<'_>, slot: &SessionSlot) -> Markup {
    let live = Live::lock(home.live).snapshot();
    let roster = home.surface.roster();
    let agents = home.surface.agents();
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
        .find(|seat| seat.name == slot.project())
        .and_then(|seat| roster.chip_for(&seat.key))
        .map(|chip| chip.account_name);
    let waking = !roster.has_agent(slot);

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "forge \u{b7} " (slot.project()) " \u{b7} " (name) }
                (root_block(home.theme))
                link rel="stylesheet" href="/web.css";
                link rel="icon" href="/favicon.svg" type="image/svg+xml";
            }
            // The checkboxes are the pane state: CSS-only, so a collapsed
            // pane needs no script and a reload does not forget it while
            // the page is open. Checked is COLLAPSED.
            body {
                input type="checkbox" id="l" hidden;
                input type="checkbox" id="r" hidden;
                div .app {
                    aside .rail .left {
                        div .banner {
                            span .t { "projects" }
                            span .n .ml { (fleet_count(&roster)) }
                        }
                        div .scroll { (rail(home, &roster, slot).await) }
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
                        div .conv { (chat_body(waking)) }
                    }
                    aside .rail .right {
                        div .banner {
                            span .t { "inspector" }
                            span .n .ml { (slot.project()) }
                        }
                        div .scroll {}
                    }
                }
            }
        }
    }
}

/// The projects rail: every declared project, grouped by the strongest
/// state among its own rows, with its workers under it.
async fn rail(home: &Home<'_>, roster: &Roster, slot: &SessionSlot) -> Markup {
    let unseen = Live::lock(home.live).snapshot().unseen;
    let agents = home.surface.agents();
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
/// chip is drawn unavailable with the reason rather than as a control that
/// does nothing when it is clicked.
fn close_chip() -> Markup {
    html! {
        span .x aria-disabled="true"
             title="closing a session from here needs the dispatch path, which lands separately" {
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

/// What the chat column says while the conversation itself is not there
/// yet: the seat is connecting, and that is the whole of what is known.
fn chat_body(waking: bool) -> Markup {
    if !waking {
        return Markup::default();
    }
    html! {
        div .hold {
            span .ring {}
            "connecting\u{2026}"
            span .sub { "nothing is running on this seat yet" }
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
