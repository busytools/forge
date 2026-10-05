//! The composer: the box a message is typed into, the list that opens
//! over it, the take it can dictate, the states that replace it when the
//! seat cannot take input, and the dock a prompt morphs it into.
//!
//! Two inputs and no others. What the reader is doing - the draft - arrives
//! on the request, because the browser is what holds it. Everything else is
//! a read of the core through the view surface, or something the stream said
//! once and did not keep, which [`Composer`] holds.

use std::time::Instant;

use forge_primitives::SessionSlot;
use forge_primitives::permission_interaction::{
    PermissionOptionKind, PermissionOutcome, PermissionRequest,
};
use forge_primitives::question::{QuestionAnnotation, QuestionOutcome, QuestionRequest};
use forge_primitives::session_update::ToolCall;
use forge_server::composer::{Composer, Notice, Phase, SignIn, Take};
use forge_server::file_index::FileIndex;
use forge_server::live::Live;
// One prompt as the composer draws it, whichever copy it came from: the
// wire's, or the one the core kept beside the answer's oneshot for a view
// that attached after it landed.
use forge_server::surface::PendingAsk as Ask;
use forge_server::surface::{AgentRow, Agents, PendingKind, Roster, ViewSurface};
use maud::{Markup, html};

use crate::home::{Home, State};
use crate::icons;

/// How many file rows the `@` list shows, matching the TUI's own cap. The
/// two views cannot name each other's constants, so the number is declared
/// here as well as there.
const FILE_ROWS: usize = 32;

/// How many agent rows the `&` list shows, matching the TUI's own.
const AGENT_ROWS: usize = 8;

/// How many candidates a list is ranked down to before it is drawn, which is
/// the TUI's own cap. It is not how many rows are visible: those scroll in a
/// window, and this is the set the window scrolls over.
const CANDIDATES: usize = 200;

/// The draft a request carries, decoded.
pub(crate) fn draft_of(raw_query: Option<&str>) -> String {
    field(raw_query.unwrap_or_default(), "draft").unwrap_or_default()
}

/// One form field out of a request, decoded. A browser escapes a form field
/// with `%XX` and `+`, so the value a control sent is the one the server
/// reads, byte for byte. The query and the body are the same shape, so both
/// come through here.
pub(crate) fn field(raw: &str, name: &str) -> Option<String> {
    let value = raw
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))?;
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.bytes();
    while let Some(byte) = chars.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => match (chars.next().and_then(hex), chars.next().and_then(hex)) {
                (Some(high), Some(low)) => bytes.push(high << 4 | low),
                _ => bytes.push(b'%'),
            },
            other => bytes.push(other),
        }
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The command one option of a held prompt answers with, or `None` when this
/// view is not the one holding it.
///
/// The outcome is built here from the option the core offered rather than
/// from anything the browser sent: an option's own action is what the CLI
/// decides on, and a browser naming its own could allow what the prompt
/// never offered.
pub(crate) fn answer(
    held: &Composer,
    kept: &[Ask],
    slot: &SessionSlot,
    tool_id: &str,
    option_id: Option<&str>,
    notes: Option<&str>,
) -> Option<forge_server::Command> {
    // Resolved the way the render resolves it, and for the same reason: a
    // view that attached after the prompt landed, or one holding several
    // prompts, has only the core's copy of the one this form names.
    let ask = held
        .asks(slot)
        .iter()
        .find(|ask| ask.tool_id() == Some(tool_id))
        .or_else(|| kept.iter().find(|ask| ask.tool_id() == Some(tool_id)))?;
    match ask {
        Ask::Permission(request) if request.tool_call.tool_call_id == tool_id => {
            // The option is looked up in the core's own list, never taken
            // from the request: an id the prompt never offered yields
            // nothing rather than an outcome the browser chose.
            let option = request
                .options
                .iter()
                .find(|option| Some(option.option_id.as_str()) == option_id)?;
            Some(forge_server::Command::RespondPermission {
                key: slot.clone(),
                tool_id: tool_id.to_owned(),
                outcome: PermissionOutcome::Selected {
                    option_id: option.option_id.clone(),
                    action: option.action.clone(),
                    notes_text: notes.map(str::to_owned),
                    edited_input: None,
                },
            })
        }
        Ask::Question(request) if request.tool_call.tool_call_id == tool_id => {
            // The same lookup the permission arm makes: a question's options
            // are the core's, so an id it never offered is not an answer.
            let chosen = match option_id {
                Some(option_id) => request
                    .prompt
                    .options
                    .iter()
                    .find(|option| option.option_id == option_id)
                    .map(|option| vec![option.option_id.clone()])?,
                None => Vec::new(),
            };
            let annotation = notes
                .filter(|notes| !notes.trim().is_empty())
                .map(|notes| QuestionAnnotation { preview: None, notes: Some(notes.to_owned()) });
            // The TUI's own rule: nothing chosen and nothing said is not an
            // answer, it is a rejection.
            let outcome = if chosen.is_empty() && annotation.is_none() {
                QuestionOutcome::Cancelled
            } else {
                QuestionOutcome::Answered { selected_option_ids: chosen, annotation }
            };
            Some(forge_server::Command::RespondQuestion {
                key: slot.clone(),
                tool_id: tool_id.to_owned(),
                outcome,
            })
        }
        _ => None,
    }
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// What a render knows about the field it draws.
///
/// The browser holds the text, so only the request that carried it, and the
/// send that took it, know what it says. A push knows neither, and that is
/// the state the box has to be drawn around: the field keeps its element and
/// the controls its text earned stay drawn, because nothing else submits
/// this form and a push that dropped them would take the only way to send.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Draft {
    /// The request carried the field's text.
    Known,
    /// A push: the browser holds the text and the server does not.
    Unknown,
    /// The send took the words, so the field is drawn empty to match.
    Cleared,
}

/// Render the composer for `slot`, with `draft` standing in the box.
pub fn render(
    home: &Home<'_>,
    slot: &SessionSlot,
    roster: &Roster,
    agents: &Agents,
    draft: &str,
    draft_state: Draft,
) -> Markup {
    let live = Live::lock(home.live).snapshot();
    let held = &live.composer;
    let row = agents.all().iter().find(|row| &row.slot == slot);
    let state =
        row.map_or(State::NeverStarted, |row| crate::home::state_of_agent(row, &live.unseen));
    // Two bases, and the difference is the whole of it: the box fetches its
    // own region, and a control posts to the seat it belongs to. One base
    // for both is a URL no route serves, and htmx swaps nothing on a 404, so
    // the click looks like nothing happened.
    let seat = crate::session::href(slot);
    let region = format!("{seat}/composer");
    // The stream's copy is the newest. The core's is the one a view that
    // attached after the prompt landed has at all: the wire carried it once
    // and kept it nowhere else.
    let kept = home.surface.pending_asks(slot);

    html! {
        form #comp .comp hx-get=(region) hx-target="#comp" hx-swap="outerHTML"
            hx-trigger="input delay:150ms" {
            @if let Some(blocked) = blocked(state, row, slot, held) {
                (blocked_box(&blocked, draft_state))
            } @else if let Some(pending) = row.and_then(|row| row.pending) {
                (dock(row, pending, held.ask(slot).or_else(|| kept.first()), &seat, draft_state))
            } @else {
                (hint(row, held.sign_in(slot)))
                (popover(home, slot, roster, draft))
                (box_markup(
                    draft,
                    held.take(slot),
                    held.notice(slot),
                    dictation_offered(home),
                    &seat,
                    draft_state,
                ))
            }
        }
    }
}

// ---------- the states that replace the box ----------

/// A state that replaces the box entirely: the seat is not taking input,
/// and this is why.
struct Blocked {
    line: String,
    /// What to do about it, when there is something to do.
    sub: Option<String>,
    /// Whether the line reports a failure rather than a wait.
    bad: bool,
}

/// The reason the seat takes no input, or `None` when it does.
///
/// The order is the one a reader has to be told: a seat with nothing behind
/// it cannot tell whether a session is coming up or never started, so the
/// line says which of the two the core reports rather than claiming a
/// connection no spawn is making.
fn blocked(
    state: State,
    row: Option<&AgentRow>,
    slot: &SessionSlot,
    held: &Composer,
) -> Option<Blocked> {
    use forge_primitives::SessionLifecycleState as Lifecycle;

    let reason = || {
        row.and_then(|row| row.reason.clone())
            .unwrap_or_else(|| "this seat has no session behind it".to_owned())
    };
    match state {
        // Sign-in is the one state that keeps its box: the hint above it is
        // where the reader is told, which is the mockup's own shape for it.
        State::Lifecycle(Lifecycle::AuthRequired) => None,
        State::Lifecycle(Lifecycle::Failed) => Some(Blocked {
            line: "could not start".to_owned(),
            sub: row.and_then(|row| row.reason.clone()),
            bad: true,
        }),
        State::NeverStarted | State::Lifecycle(Lifecycle::Sleeping | Lifecycle::LoggedOut) => {
            Some(Blocked { line: "not running".to_owned(), sub: Some(reason()), bad: false })
        }
        State::Lifecycle(Lifecycle::Spawning) => Some(Blocked {
            line: "Connecting to Claude Code\u{2026}".to_owned(),
            sub: None,
            bad: false,
        }),
        State::Lifecycle(Lifecycle::Running | Lifecycle::Idle | Lifecycle::Attention)
        | State::Unseen => held.compacting(slot).then(|| Blocked {
            line: "Compacting context\u{2026}".to_owned(),
            sub: None,
            bad: false,
        }),
    }
}

fn blocked_box(blocked: &Blocked, draft_state: Draft) -> Markup {
    html! {
        div class=(if blocked.bad { "box err" } else { "box" }) {
            div .blocked {
                (parked_field(draft_state))
                span .b1 {
                    @if !blocked.bad {
                        span .ring {}
                    }
                    (&blocked.line)
                }
                @if let Some(sub) = &blocked.sub {
                    span .b2 { (sub) }
                }
            }
        }
    }
}

/// The line above the box, for what the reader has to know before typing.
///
/// One hint: a seat that needs signing in cannot get anything sent from it
/// anywhere, and the wire names the method it is waiting on, so the line can
/// say which sign-in rather than only that there is one.
fn hint(row: Option<&AgentRow>, sign_in: Option<&SignIn>) -> Markup {
    use forge_primitives::SessionLifecycleState as Lifecycle;

    if row.map(|row| row.lifecycle) != Some(Lifecycle::AuthRequired) {
        return Markup::default();
    }
    html! {
        div .hint .login {
            "Authentication required"
            @if let Some(sign_in) = sign_in {
                " \u{b7} " (&sign_in.method_name)
            }
            span .sub {
                @match sign_in {
                    Some(sign_in) if !sign_in.method_description.is_empty() => {
                        (&sign_in.method_description)
                    }
                    _ => "Run `claude auth login` in another terminal to authenticate",
                }
            }
        }
    }
}

// ---------- the box ----------

/// Whether this install can dictate at all. An install with `[dictate]` off
/// loads no models, and a control it cannot honour is worse than none.
fn dictation_offered(home: &Home<'_>) -> bool {
    !home.surface.dictate().snapshot.models.is_empty()
}

/// The box itself, in whichever of its states the seat is in.
fn box_markup(
    draft: &str,
    take: Option<&Take>,
    notice: Option<&Notice>,
    dictation: bool,
    endpoint: &str,
    draft_state: Draft,
) -> Markup {
    // A landed take puts its words where the reader was about to type, so
    // the box holds the draft and the words together. Nothing else changes
    // the draft.
    let (draft, landed) = match notice {
        Some(Notice::Landed { text, .. }) => (joined(draft, text), true),
        _ => (draft.to_owned(), false),
    };
    let state = match (take, landed) {
        (Some(take), _) => match take.phase {
            Phase::Recording => " rec",
            Phase::Transcribing => " tr",
        },
        (None, true) => " done",
        (None, false) => "",
    };
    // A push does not know the reader's text, and the field it keeps may
    // hold some, so what that text earned stays drawn. Whitespace is not
    // text: the send route refuses it and the button would advertise a
    // click that cannot happen.
    let filled = draft_state == Draft::Unknown || !draft.trim().is_empty();
    html! {
        div class=(format!("box{state}")) {
            @if let Some(take) = take {
                (dictation_row(take, endpoint))
            } @else if let Some(notice) = notice.filter(|notice| notice.line().is_some()) {
                div class=(format!("notice {}", notice.tone())) { (notice.line().unwrap_or_default()) }
            }
            div .line {
                textarea #draft .txt name="draft" rows="3" autocomplete="off" spellcheck="false"
                    hx-preserve[draft_state != Draft::Cleared]
                    placeholder="Type a message\u{2026}" { (&draft) }
                @if filled {
                    button .send type="submit" hx-post=(format!("{endpoint}/send"))
                        hx-include="#draft" hx-target="#comp" hx-swap="outerHTML"
                        title="send" {
                        (icons::icon("send", ""))
                    }
                }
            }
            @if filled || dictation {
                div .foot {
                    @if filled {
                        span .k { "\u{21b5}" } " send "
                        span .k { "\u{21e7}\u{21b5}" } " newline"
                    }
                    @if dictation {
                        (mic_control(endpoint, take.is_some()))
                    }
                }
            }
        }
    }
}

/// The control that starts a take, and while one runs, the one that submits
/// it - the TUI's own rule, where the key that opens a take is the key that
/// closes it. The mockup draws no way in, since every take it draws is
/// already running, so the affordance is here rather than missing.
fn mic_control(endpoint: &str, running: bool) -> Markup {
    let action = if running { "stop" } else { "start" };
    html! {
        button .mic type="submit" hx-post=(format!("{endpoint}/dictate"))
            hx-vals=(format!(r#"{{"action":"{action}"}}"#))
            hx-target="#comp" hx-swap="outerHTML"
            title=(if running { "submit the take" } else { "start a take" }) {
            (icons::icon("mic", ""))
        }
    }
}

/// The draft with a take's words at its end, which is where the caret was.
fn joined(draft: &str, words: &str) -> String {
    if draft.is_empty() || draft.ends_with(char::is_whitespace) {
        format!("{draft}{words}")
    } else {
        format!("{draft} {words}")
    }
}

// ---------- dictation ----------

fn dictation_row(take: &Take, endpoint: &str) -> Markup {
    let tone = if take.phase == Phase::Transcribing { " tr" } else { "" };
    let label = match (take.phase, take.progress) {
        (_, (done, Some(total))) => format!("transcribing {done}/{total}"),
        (Phase::Transcribing, _) => "transcribing".to_owned(),
        (Phase::Recording, _) => "listening".to_owned(),
    };
    html! {
        div .dict {
            span class=(format!("dot{tone}")) {}
            span class=(format!("t{tone}")) { (clock(take.started)) }
            span class=(format!("db{tone}")) { (format!("{:.0} dB", take.peak_db)) }
            span class=(format!("wave{tone}")) {
                span .wtr {
                    @for level in &take.levels {
                        i class=(level_tone(*level))
                          style=(format!("height:{:.0}%", Take::height(*level))) {}
                    }
                }
            }
            span .lbl { (label) }
            button .esc type="submit" hx-post=(format!("{endpoint}/dictate"))
                hx-vals=r#"{"action":"cancel"}"# hx-target="#comp" hx-swap="outerHTML"
                title="abandon the take" { "esc cancel" }
        }
    }
}

/// How a reading sits on the meter's own colour ramp. The thresholds are
/// the mockup's.
fn level_tone(level: f32) -> &'static str {
    if level >= 0.7 {
        "hot"
    } else if level >= 0.4 {
        "mid"
    } else {
        ""
    }
}

/// How long the take has been running, as a reader reads it.
fn clock(started: Instant) -> String {
    let seconds = started.elapsed().as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

// ---------- the popover ----------

/// Which list a draft has open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Command,
    File,
    Agent,
}

/// The trigger a draft ends in, with what it is matching on. The token is
/// the text after the last space, which is where a caret at the end of the
/// draft leaves it.
fn trigger_of(draft: &str) -> Option<(Trigger, &str)> {
    let token = match draft.char_indices().rev().find(|(_, c)| c.is_whitespace()) {
        Some((index, c)) => &draft[index + c.len_utf8()..],
        None => draft,
    };
    if let Some(query) = token.strip_prefix('/') {
        // A command is the whole draft while it is being typed, which is
        // what keeps a slash inside a sentence a path rather than a
        // command.
        return (token == draft).then_some((Trigger::Command, query));
    }
    if let Some(query) = token.strip_prefix('@') {
        return (!query.is_empty()).then_some((Trigger::File, query));
    }
    if let Some(query) = token.strip_prefix('&') {
        return (!query.is_empty()).then_some((Trigger::Agent, query));
    }
    None
}

/// The list a half-typed trigger opens.
fn popover(home: &Home<'_>, slot: &SessionSlot, roster: &Roster, draft: &str) -> Markup {
    let Some((trigger, query)) = trigger_of(draft) else {
        return Markup::default();
    };
    // What the header counts is where the list comes from - a cap, or the
    // size of the table behind it - and never how many rows survived the
    // query, so a short list reads as filtered rather than as the whole set.
    let (icon, title, cap, rows) = match trigger {
        Trigger::Command => {
            // forge's own commands first, then the ones the CLI advertised
            // that forge does not handle itself: a name in both lists is
            // forge's, and drawing the CLI's copy beside it would offer one
            // command twice.
            let forge = ViewSurface::forge_commands();
            let advertised: Vec<forge_primitives::AvailableCommand> = home
                .surface
                .slash_commands(slot)
                .iter()
                .filter(|command| !forge_server::commands::is_forge_command(&command.name))
                .cloned()
                .collect();
            let count = forge.len() + advertised.len();
            let rows: Vec<(String, Markup)> = forge
                .iter()
                .map(|command| (command.name, command.description))
                .chain(
                    advertised
                        .iter()
                        .map(|command| (command.name.as_str(), command.description.as_str())),
                )
                .filter(|(name, description)| matches_query(&[name, description], query))
                .take(CANDIDATES)
                // Every name arrives with its slash on, forge's table and the
                // CLI's list alike, so the row's own text is what a pick
                // writes.
                .map(|(name, description)| (name.to_owned(), row(name, description, None, query)))
                .collect();
            ("cmd", "commands".to_owned(), count.to_string(), rows)
        }
        Trigger::File => {
            // **This page holds no seat, so no seat's loop walks for it.**
            // The socket's clients read the store a held seat's loop writes;
            // a page served from the process with no subscription behind it
            // has no such store, and walks the tree where it stands.
            let index = match roster.cwd_for(slot) {
                Some(cwd) => home.surface.walk_file_index(std::path::Path::new(&cwd)),
                None => FileIndex::default(),
            };
            let found = index.visible(query, FILE_ROWS);
            let rows: Vec<(String, Markup)> = found
                .iter()
                .map(|file| (format!("@{}", file.rel_path), row(&file.rel_path, "", None, query)))
                .collect();
            ("file", "files & folders".to_owned(), format!("{FILE_ROWS} max"), rows)
        }
        Trigger::Agent => {
            let agents = home.surface.subagents(slot);
            let rows: Vec<(String, Markup)> = agents
                .iter()
                .filter(|agent| matches_query(&[&agent.name, &agent.description], query))
                .take(AGENT_ROWS)
                .map(|agent| {
                    (format!("&{}", agent.name), row(&agent.name, &agent.description, None, query))
                })
                .collect();
            ("bot", "subagents".to_owned(), format!("{AGENT_ROWS} max"), rows)
        }
    };
    // Nothing matched, so there is nothing to open on: a popover with only
    // a header would read as a list that lost its rows.
    if rows.is_empty() {
        return Markup::default();
    }
    html! {
        div .ac {
            div .h {
                (icons::icon(icon, ""))
                (title)
                span .n { (cap) }
            }
            // The rows scroll inside a window of their own rather than
            // growing the popover: the box sits under it, and a list drawn
            // in full pushes the box past the viewport, where a page that
            // is one viewport tall clips it and the reader types blind.
            div .rows {
                @for (index, (insert, row)) in rows.into_iter().enumerate() {
                    div class=(if index == 0 { "it sel" } else { "it" }) data-ins=(insert) {
                        span .cur { @if index == 0 { "\u{25b8}" } }
                        (row)
                    }
                }
            }
            // The keys the list answers to, which are the page's own: the
            // row a pick writes is the one marked, and what it writes is the
            // value its `data-ins` carries rather than the drawing here.
            div .keys {
                span { span .k { "\u{2191}\u{2193}" } " select" }
                span { span .k { "\u{21b5}" } " choose" }
                span { span .k { "esc" } " close" }
            }
        }
    }
}

/// Whether a row's own text is what has been typed. A list is filtered and
/// never left whole, or a query would look like it did nothing.
fn matches_query(fields: &[&str], query: &str) -> bool {
    let query = query.to_lowercase();
    fields.iter().any(|field| field.to_lowercase().contains(&query))
}

/// One row of a list: what the row is, the marked span the list matched on,
/// and whatever the surface holds beside it.
fn row(text: &str, detail: &str, glyph: Option<String>, query: &str) -> Markup {
    html! {
        @if let Some(glyph) = glyph {
            span .g { (glyph) }
        }
        span .p { (marked(text, query)) }
        @if !detail.is_empty() {
            span .d { (detail) }
        }
    }
}

/// `text` with the query's own span marked, which is what the list matched
/// it on. Nothing is marked when the query is empty, which is what a bare
/// trigger shows.
fn marked(text: &str, query: &str) -> Markup {
    let lower = text.to_lowercase();
    // A query whose lowercase changes the byte count cannot be found by
    // offset, and marking the wrong span is worse than marking none.
    let located = (lower.len() == text.len() && !query.is_empty())
        .then(|| lower.find(&query.to_lowercase()))
        .flatten();
    let Some(at) = located else {
        return html! { (text) };
    };
    let end = at + query.len();
    html! {
        (text.get(..at).unwrap_or_default())
        em { (text.get(at..end).unwrap_or_default()) }
        (text.get(end..).unwrap_or_default())
    }
}

// ---------- the dock ----------

/// The prompt dock: the box morphed, because the eye is already there.
///
/// Which prompt waits is the core's answer, read through the seat's row.
/// What it offers is the core's too: the request is kept beside the
/// answer's oneshot, so a view that attached after the prompt landed draws
/// the options from there. The fallback is the window between those two
/// reads, and it says so rather than drawing a box that would read as
/// nothing pending.
fn dock(
    row: Option<&AgentRow>,
    kind: PendingKind,
    ask: Option<&Ask>,
    endpoint: &str,
    draft_state: Draft,
) -> Markup {
    html! {
        div .dock {
            (parked_field(draft_state))
            @if let Some(depth) = row.map(|row| row.pending_depth).filter(|depth| *depth > 1) {
                div .queue { "\u{25bc} " (depth - 1) " more pending" }
            }
            @match ask {
                Some(Ask::Permission(request)) => (permission_dock(request, endpoint)),
                Some(Ask::Question(request)) => (question_dock(request, endpoint)),
                // A draft reads back through `pending_asks` now, and this
                // page has no dock for one - it drew the unknown line for a
                // seat holding a draft before, and it still does. The dock
                // is the client's, and this crate stops being started.
                Some(Ask::SlackDraft(_)) | None => (unknown_dock(kind)),
            }
            (dock_keys(ask))
        }
    }
}

/// The composer's keys, which are the page's own: every one of them drives a
/// control the page already draws, so nothing here posts anything itself.
///
/// Nothing binds to a node. The region is replaced under the reader - by a
/// push, or by the page's own GET while they type - so one listener on the
/// document reads whatever is drawn now, and the three surfaces it serves
/// are the box, the prompt dock and the list.
pub(crate) const COMPOSER_KEYS: &str = r"
(() => {
  // A NodeList indexes and iterates but has no findIndex, so the rows are
  // spread before they are searched.
  const marked = (rows) => {
    const at = [...rows].findIndex((row) => row.classList.contains('sel'));
    return at < 0 ? 0 : at;
  };
  const move = (rows, step) => {
    if (!rows.length) return;
    const was = marked(rows);
    const next = (was + step + rows.length) % rows.length;
    rows.forEach((row, index) => {
      row.classList.toggle('sel', index === next);
      const cur = row.querySelector('.cur');
      if (cur) cur.textContent = index === next ? '▸' : '';
    });
    rows[next].scrollIntoView({ block: 'nearest' });
  };
  const startOfToken = (text) => {
    const at = text.search(/\S+$/);
    return at < 0 ? text.length : at;
  };
  document.addEventListener('keydown', (event) => {
    if (event.defaultPrevented || event.isComposing || event.altKey || event.metaKey) return;
    const comp = document.querySelector('#comp');
    if (!comp) return;
    const field = comp.querySelector('#draft');
    const typing = field && document.activeElement === field;
    const step = { ArrowDown: 1, ArrowUp: -1 }[event.key];

    const list = comp.querySelectorAll('.ac .it');
    if (list.length && typing) {
      if (step) { event.preventDefault(); move(list, step); return; }
      if (event.key === 'Escape') { event.preventDefault(); comp.querySelector('.ac').remove(); return; }
      if (event.key === 'Enter') {
        const value = list[marked(list)].dataset.ins;
        event.preventDefault();
        field.value = field.value.slice(0, startOfToken(field.value)) + value + ' ';
        field.dispatchEvent(new Event('input', { bubbles: true }));
        return;
      }
    }

    const dock = comp.querySelector('.dock');
    if (dock) {
      const opts = dock.querySelectorAll('.opt');
      const notes = dock.querySelector('textarea.notes');
      // A question's own words field is where the reader is typing, and
      // Enter there submits what they wrote rather than the marked option -
      // the TUI's own rule for its Notes row. Any other field on the page
      // keeps its keys: this listener is for the dock, not for the page.
      if (notes && document.activeElement === notes) {
        if (event.key === 'Enter') {
          const own = dock.querySelector('.lbl.own');
          if (own) { event.preventDefault(); own.click(); }
        }
        return;
      }
      if (document.activeElement
        && (document.activeElement.tagName === 'TEXTAREA'
          || document.activeElement.tagName === 'INPUT')) {
        return;
      }
      if (step) { event.preventDefault(); move(opts, step); return; }
      if (event.key === 'Enter') {
        const control = opts[marked(opts)] && opts[marked(opts)].querySelector('.lbl');
        if (control) { event.preventDefault(); control.click(); }
        return;
      }
      if (event.key === 'Escape') {
        const reject = [...opts].find((opt) => opt.querySelector('.no'));
        const control = reject && reject.querySelector('.lbl');
        if (control) { event.preventDefault(); control.click(); }
        return;
      }
    }

    if (typing && event.key === 'Enter' && !event.shiftKey) {
      const send = comp.querySelector('.send');
      if (send) { event.preventDefault(); send.click(); }
    }
  });
})();";

/// The keys line the mockup draws under a dock's options: what the page
/// reads while the prompt is up.
///
/// A question's line drops the mockup's toggle. Its rows draw the boxes the
/// mockup draws and the answer the page can send carries one option, so a
/// toggle would name a key that cannot do what it says, which is the defect
/// the line exists to avoid.
fn dock_keys(ask: Option<&Ask>) -> Markup {
    // A dock with no offer draws no rows to move between, so it names no
    // keys: the line is a promise about what the dock below it answers to.
    let Some(ask) = ask else {
        return Markup::default();
    };
    let question = matches!(ask, Ask::Question(_));
    let (moved, confirmed) = if question { ("move", "submit") } else { ("select", "confirm") };
    html! {
        div .keys {
            span { span .k { "\u{2191}\u{2193}" } " " (moved) }
            span { span .k { "\u{21b5}" } " " (confirmed) }
            @if !question {
                span { span .k { "esc" } " reject" }
            }
        }
    }
}

/// The field the box was drawn with, kept while the dock is up.
///
/// A push cannot carry the reader's text - the browser holds it - and a node
/// the new markup does not have is removed, so the field has to be present
/// for the browser to keep its own, with the same id and the same preserve
/// marker. The sheet hides it under the dock and the box shows it again.
///
/// It is drawn with the box's own attributes rather than as a bare slot,
/// because which node ends up in the box is the browser's business: a page
/// whose first render is a parked one - a seat that has not started, or one
/// already holding a prompt - hands this node to the box, and a field with
/// no name contributes nothing to `hx-include` and would send nothing.
fn parked_field(draft_state: Draft) -> Markup {
    html! {
        textarea #draft .txt name="draft" rows="3" autocomplete="off" spellcheck="false"
            hx-preserve[draft_state != Draft::Cleared]
            placeholder="Type a message\u{2026}" {}
    }
}

/// A prompt the core reports and this view never saw the offer of.
fn unknown_dock(kind: PendingKind) -> Markup {
    let what = match kind {
        PendingKind::Question => "a question is waiting for you",
        PendingKind::Permission => "a permission prompt is waiting",
    };
    html! {
        div .head { span .t { (what) } }
        div .desc { "its options arrived before this view attached" }
    }
}

fn permission_dock(request: &PermissionRequest, endpoint: &str) -> Markup {
    let call = &request.tool_call;
    let display = request.display.clone().unwrap_or_default();
    let title = display
        .display_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| display.title.clone().filter(|name| !name.trim().is_empty()))
        .unwrap_or_else(|| call.title.clone());
    let subject = subject_of(call);
    html! {
        div .head {
            span .t .mono { (title) }
            @if !subject.is_empty() {
                span .q { (subject) }
            }
        }
        @if let Some(reason) = display.decision_reason.filter(|line| !line.trim().is_empty()) {
            div .reason { (reason) }
        }
        @if let Some(description) = display.description.filter(|line| !line.trim().is_empty()) {
            div .desc { (description) }
        }
        div .opts {
            @for (index, option) in request.options.iter().enumerate() {
                div class=(opt_class(index)) {
                    span .cur { @if index == 0 { "\u{25b8}" } }
                    (icons::icon(option_icon(option.kind), option_tone(option.kind)))
                    (option_control(&option.name, endpoint, &call.tool_call_id, &option.option_id))
                }
            }
        }
    }
}

fn question_dock(request: &QuestionRequest, endpoint: &str) -> Markup {
    let prompt = &request.prompt;
    let tool_id = &request.tool_call.tool_call_id;
    html! {
        div .head {
            span .qm { "?" }
            span .t { (&prompt.header) }
            span .q {
                (format!("Q{} of {}", request.question_index + 1, request.total_questions))
            }
        }
        div .desc { (&prompt.question) }
        div .opts {
            @for (index, option) in prompt.options.iter().enumerate() {
                div class=(opt_class(index)) {
                    span .cur { @if index == 0 { "\u{25b8}" } }
                    span .box2 {}
                    (option_control(&option.label, endpoint, tool_id, &option.option_id))
                }
            }
            // The escape hatch the mockup draws: it answers the question
            // with words instead of a choice, which is an answer the CLI
            // accepts with nothing selected.
            div .opt {
                span .cur {}
                span .box2 {}
                button .lbl .own type="submit" hx-post=(format!("{endpoint}/answer"))
                    hx-include="closest .dock" hx-vals=(format!(r#"{{"tool_id":"{tool_id}"}}"#))
                    hx-target="#comp" hx-swap="outerHTML" {
                    "Tell the agent something else:"
                }
            }
        }
        // What the reader has typed here is theirs, and a push cannot carry
        // it, so the field is preserved like the box's own. The id names the
        // question rather than the field: words written for one question are
        // not an answer to the next, and a fresh id is a fresh field.
        textarea .notes id=(format!("notes-{tool_id}"))
            name="notes" rows="1" hx-preserve
            placeholder="answer with your own words" {}
    }
}

/// What an option's row carries: the first is the one a key would take.
fn opt_class(index: usize) -> &'static str {
    match index {
        0 => "opt sel",
        _ => "opt",
    }
}

/// One option, as the control it is: it answers the prompt it was drawn for,
/// and names the option the core offered rather than the outcome, which only
/// the core may build.
fn option_control(label: &str, endpoint: &str, tool_id: &str, option_id: &str) -> Markup {
    html! {
        button .lbl type="submit" hx-post=(format!("{endpoint}/answer"))
            hx-vals=(format!(r#"{{"tool_id":"{tool_id}","option_id":"{option_id}"}}"#))
            hx-target="#comp" hx-swap="outerHTML" { (label) }
    }
}

/// Which sprite carries a permission option's meaning.
fn option_icon(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::Allow => "check",
        PermissionOptionKind::Deny => "x",
        PermissionOptionKind::Edit => "edit",
        PermissionOptionKind::Notes => "dots",
    }
}

/// The colour an option's own meaning takes, which the sheet has one rule
/// per.
fn option_tone(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::Allow => "ok",
        PermissionOptionKind::Deny => "no",
        PermissionOptionKind::Edit => "ed",
        PermissionOptionKind::Notes => "",
    }
}

/// What a tool call is about, read from the field the CLI fills for the
/// tool it named, and falling back to the input itself.
fn subject_of(call: &ToolCall) -> String {
    let Some(input) = &call.raw_input else {
        return String::new();
    };
    for key in ["command", "file_path", "url"] {
        if let Some(value) = input.get(key).and_then(serde_json::Value::as_str) {
            return value.to_owned();
        }
    }
    serde_json::to_string(input).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dock with no offer draws no rows, so it names no keys: the line is
    /// a promise about what the dock under it answers to, and a promise
    /// about keys the page swallows is the defect the line exists to avoid.
    #[test]
    fn a_dock_with_nothing_to_select_names_no_keys() {
        let permission = serde_json::from_value::<PermissionRequest>(serde_json::json!({
            "tool_call": {
                "tool_call_id": "tu-1",
                "title": "Bash",
                "kind": "execute",
                "status": "pending",
                "content": [],
                "locations": [],
                "raw_input": {"command": "ls"},
            },
            "options": [],
        }))
        .expect("a permission request off the wire");

        assert!(dock_keys(None).into_string().is_empty(), "nothing to select draws no keys line");
        let named = dock_keys(Some(&Ask::Permission(Box::new(permission)))).into_string();
        assert!(named.contains("confirm"), "and a dock with rows names theirs: {named}");
    }
}
