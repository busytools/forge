//! The session page: the projects rail, the conversation, and the
//! inspector.
//!
//! Three columns over one sheet. The rail and the inspector collapse to
//! nothing and their handles live in the chat header, so a collapsed pane
//! leaves no edge behind and its control stays reachable. Every column
//! reads the core through the view surface: this module holds no state of
//! its own.

use std::net::SocketAddr;

use forge_primitives::SessionSlot;
use forge_primitives::account::AccountAuth;
use forge_sessions::surface::{AccountsView, LoadingState, Roster};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::home::{Home, State};
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
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
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
        Found::Open(open_page(state, bound, &slot).await)
    } else {
        Found::Waking(waking_page(state, bound, &slot))
    }
}

/// The pieces both pages read the core through, which are the home's own:
/// one view context per page, over the same surface, cache and live state.
fn context<'a>(state: &'a WebState, bound: SocketAddr) -> Home<'a> {
    Home {
        surface: &state.surface,
        work: &state.work,
        live: &state.live,
        bound,
        mark: state.config.mark.as_deref(),
        theme: state.config.theme.as_deref(),
    }
}

/// A seat with a session behind it.
async fn open_page(state: &WebState, bound: SocketAddr, slot: &SessionSlot) -> Markup {
    shell(&context(state, bound), slot)
}

/// A seat with nothing behind it yet. The page is the same page: the
/// columns are as real as they are for a running session, and the chat
/// column says what it is waiting for rather than drawing an empty
/// conversation.
fn waking_page(state: &WebState, bound: SocketAddr, slot: &SessionSlot) -> Markup {
    shell(&context(state, bound), slot)
}

/// The page. Pure: everything it draws comes from the surface.
fn shell(home: &Home<'_>, slot: &SessionSlot) -> Markup {
    let live = Live::lock(home.live).snapshot();
    let roster = home.surface.roster();
    let agents = home.surface.agents();
    let accounts = home.surface.accounts();
    let row = agents.all().iter().find(|row| &row.slot == slot);
    let state = row.map_or(State::NeverStarted, |row| crate::home::state_of_agent(row, &live.unseen));
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
                        div .scroll {}
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
        State::Lifecycle(forge_primitives::SessionLifecycleState::Running)
        | State::Lifecycle(forge_primitives::SessionLifecycleState::Spawning) => "live",
        State::Lifecycle(forge_primitives::SessionLifecycleState::Idle) => "idle",
        State::Unseen => "unseen",
        State::Lifecycle(forge_primitives::SessionLifecycleState::Attention) => "needs",
        // Sign-in needed is a failure the user has to act on, and the rail
        // has no auth shape of its own.
        State::Lifecycle(forge_primitives::SessionLifecycleState::AuthRequired)
        | State::Lifecycle(forge_primitives::SessionLifecycleState::Failed) => "failed",
        State::Lifecycle(forge_primitives::SessionLifecycleState::Sleeping)
        | State::Lifecycle(forge_primitives::SessionLifecycleState::LoggedOut)
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
