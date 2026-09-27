//! The composer: the box a message is typed into, the list that opens
//! over it, the take it can dictate, the states that replace it when the
//! seat cannot take input, and the dock a prompt morphs it into.
//!
//! Two inputs and no others. What the reader is doing - the draft - arrives
//! on the request, because the browser is what holds it. Everything else is
//! a read of the core through the view surface, or something the stream said
//! once and did not keep, which [`Composer`] holds.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use forge_primitives::Message;
use forge_primitives::SessionSlot;
use forge_primitives::permission_ui::{PermissionOptionKind, PermissionOutcome, PermissionRequest};
use forge_primitives::question::{QuestionAnnotation, QuestionOutcome, QuestionRequest};
use forge_primitives::session_update::ToolCall;
use forge_sessions::SessionUpdate;
// One prompt as the composer draws it, whichever copy it came from: the
// wire's, or the one the core kept beside the answer's oneshot for a view
// that attached after it landed.
use forge_sessions::surface::PendingAsk as Ask;
use forge_sessions::surface::{AgentRow, Agents, DictateOutcome, PendingKind, Roster};
use maud::{Markup, html};

use crate::home::{Home, State};
use crate::icons;
use crate::stream::Live;

/// What a control says when the path that would carry out the click is not
/// built. Named once so the box, the dock and a take cannot drift into
/// three different apologies for the same missing half.
const NO_DISPATCH: &str = "not available yet";

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

/// How many level readings the meter keeps: at the mockup's own six pixels a
/// cell, a little over 700 pixels of track. A slot narrower than that is
/// filled edge to edge and the oldest readings clip, which is the case for a
/// session column at 1440; a wider one falls short of the left edge.
const METER_CELLS: usize = 120;

/// The top of the meter's own scale, in dBFS. A reading is measured between
/// the take's own silence floor and this.
const METER_CEILING_DB: f32 = 0.0;

/// The shortest a meter cell is drawn: a cell is a past reading rather than
/// a pulse, so the quietest one still has to be visible as one.
const METER_FLOOR_PERCENT: f32 = 12.0;

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
    slot: &SessionSlot,
    tool_id: &str,
    option_id: Option<&str>,
    notes: Option<&str>,
) -> Option<forge_sessions::Command> {
    match held.ask(slot)? {
        Ask::Permission(request) if request.tool_call.tool_call_id == tool_id => {
            let option = request
                .options
                .iter()
                .find(|option| Some(option.option_id.as_str()) == option_id)?;
            Some(forge_sessions::Command::RespondPermission {
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
            let annotation = notes
                .filter(|notes| !notes.trim().is_empty())
                .map(|notes| QuestionAnnotation { preview: None, notes: Some(notes.to_owned()) });
            let selected: Vec<String> = option_id.into_iter().map(str::to_owned).collect();
            // The TUI's own rule: nothing chosen and nothing said is not an
            // answer, it is a rejection.
            let outcome = if selected.is_empty() && annotation.is_none() {
                QuestionOutcome::Cancelled
            } else {
                QuestionOutcome::Answered { selected_option_ids: selected, annotation }
            };
            Some(forge_sessions::Command::RespondQuestion {
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

/// Render the composer for `slot`, with `draft` standing in the box.
pub async fn render(
    home: &Home<'_>,
    slot: &SessionSlot,
    roster: &Roster,
    agents: &Agents,
    draft: &str,
) -> Markup {
    let live = Live::lock(home.live).snapshot();
    let held = &live.composer;
    let row = agents.all().iter().find(|row| &row.slot == slot);
    let state =
        row.map_or(State::NeverStarted, |row| crate::home::state_of_agent(row, &live.unseen));
    let endpoint = format!("{}/composer", crate::session::href(slot));
    // The stream's copy is the newest. The core's is the one a view that
    // attached after the prompt landed has at all: the wire carried it once
    // and kept it nowhere else.
    let kept = home.surface.pending_ask(slot);

    html! {
        form #comp .comp hx-get=(endpoint) hx-target="#comp" hx-swap="outerHTML"
            hx-trigger="input delay:150ms" {
            @if let Some(blocked) = blocked(state, row, slot, held) {
                (blocked_box(&blocked))
            } @else if let Some(pending) = row.and_then(|row| row.pending) {
                (dock(row, pending, held.ask(slot).or(kept.as_ref()), &endpoint))
            } @else {
                (hint(row, held.sign_in(slot)))
                (popover(home, slot, roster, draft).await)
                (box_markup(
                    draft,
                    held.take(slot),
                    held.notice(slot),
                    dictation_offered(home),
                    &endpoint,
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

fn blocked_box(blocked: &Blocked) -> Markup {
    html! {
        div class=(if blocked.bad { "box err" } else { "box" }) {
            div .blocked {
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
    let filled = !draft.is_empty();
    html! {
        div class=(format!("box{state}")) {
            @if let Some(take) = take {
                (dictation_row(take, endpoint))
            } @else if let Some(notice) = notice.filter(|notice| notice.line().is_some()) {
                div class=(format!("notice {}", notice.tone())) { (notice.line().unwrap_or_default()) }
            }
            div .line {
                textarea #draft .txt name="draft" rows="3" autocomplete="off" spellcheck="false"
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
                        (mic_control(endpoint))
                    }
                }
            }
        }
    }
}

/// The control that starts a take. The mockup draws no way in - every take
/// it draws is already running - so the affordance is here rather than
/// missing.
fn mic_control(endpoint: &str) -> Markup {
    html! {
        button .mic type="submit" hx-post=(format!("{endpoint}/dictate"))
            hx-vals=r#"{"action":"start"}"# hx-target="#comp" hx-swap="outerHTML"
            title="start a take" {
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

/// What a take is doing, as the composer draws it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Recording,
    Transcribing,
}

/// A dictation take, as the stream reported it. Folded in `Composer::apply`
/// because the wire announces each take once and retains nothing.
#[derive(Clone)]
struct Take {
    /// The take's own number among the seat's takes. A resolver or a
    /// progress report for an older one is about a take that is over.
    generation: u64,
    phase: Phase,
    /// The take's own silence floor, which the meter measures against
    /// rather than assuming one.
    floor_db: f32,
    /// Newest reading last, each a fraction of the take's own range.
    levels: VecDeque<f32>,
    /// The newest reading in dBFS, for the row's own figure.
    peak_db: f32,
    /// Settled segments, and their total once the take is closed. A live
    /// take cannot know its own total.
    progress: (usize, Option<usize>),
    started: Instant,
}

impl Take {
    fn new(floor_db: f32, generation: u64) -> Self {
        Self {
            generation,
            phase: Phase::Recording,
            floor_db,
            levels: VecDeque::new(),
            peak_db: floor_db,
            progress: (0, None),
            started: Instant::now(),
        }
    }

    /// One reading, as a fraction of the take's own range. A non-finite
    /// reading is silence rather than a level: the floor stands in for it,
    /// which is what makes structural silence read as the floor rather
    /// than as a spike.
    fn push(&mut self, peak_db: f32) {
        let reading = if peak_db.is_finite() { peak_db } else { self.floor_db };
        self.peak_db = reading;
        let span = (METER_CEILING_DB - self.floor_db).max(1.0);
        let fraction = ((reading - self.floor_db) / span).clamp(0.0, 1.0);
        if self.levels.len() >= METER_CELLS {
            self.levels.pop_front();
        }
        self.levels.push_back(fraction);
    }

    /// How tall the meter draws `level`, as a percentage of the bar.
    fn height(level: f32) -> f32 {
        METER_FLOOR_PERCENT + level * (100.0 - METER_FLOOR_PERCENT - 4.0)
    }
}

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

/// What a finished take left behind.
#[derive(Clone)]
enum Notice {
    /// The take's words, which the box takes at the caret. `truncated`
    /// means the take hit its cap and is partial.
    Landed { text: String, truncated: bool },
    /// A line about a take that produced nothing to insert.
    Line { tone: &'static str, text: String },
}

impl Notice {
    /// The notice a finished take leaves, worded per outcome. A take that
    /// landed leaves words rather than a line, and one the reader abandoned
    /// leaves nothing at all.
    fn of(outcome: &DictateOutcome, floor_db: f32) -> Option<Self> {
        let line = |tone, text: String| Some(Self::Line { tone, text });
        match outcome {
            DictateOutcome::Landed { text, truncated } => {
                Some(Self::Landed { text: text.clone(), truncated: *truncated })
            }
            DictateOutcome::Cancelled => None,
            DictateOutcome::Empty => {
                line("q", "that was all filler \u{b7} nothing to insert".to_owned())
            }
            DictateOutcome::NoAudio { peak_db, seconds } if peak_db.is_finite() => line(
                "q",
                format!(
                    "nothing above {} dBFS in {seconds}s \u{b7} loudest was {peak_db:.1} \
                     \u{b7} try again",
                    floor_db.round()
                ),
            ),
            DictateOutcome::NoAudio { .. } => line(
                "bad",
                "no signal from the microphone at all \u{b7} check permission or mute".to_owned(),
            ),
            DictateOutcome::Refused { message } => line("bad", message.clone()),
            DictateOutcome::Failed => line(
                "q",
                "dictation failed \u{b7} try again; restart forge if it repeats".to_owned(),
            ),
        }
    }

    /// The line this notice draws, if it draws one. A landed take draws its
    /// words in the box instead.
    fn line(&self) -> Option<String> {
        match self {
            Self::Landed { truncated: true, .. } => {
                Some("this is what fitted \u{b7} keep going from the end".to_owned())
            }
            Self::Landed { .. } => None,
            Self::Line { text, .. } => Some(text.clone()),
        }
    }

    fn tone(&self) -> &'static str {
        match self {
            Self::Landed { .. } => "warn",
            Self::Line { tone, .. } => tone,
        }
    }
}

// ---------- the popover ----------

/// Which list a draft has open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Command,
    File,
    Agent,
    Emoji,
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
    if let Some(query) = token.strip_prefix(':') {
        return (query.chars().count() >= forge_sessions::emoji::MIN_QUERY_CHARS)
            .then_some((Trigger::Emoji, query));
    }
    None
}

/// The list a half-typed trigger opens.
async fn popover(home: &Home<'_>, slot: &SessionSlot, roster: &Roster, draft: &str) -> Markup {
    let Some((trigger, query)) = trigger_of(draft) else {
        return Markup::default();
    };
    // What the header counts is where the list comes from - a cap, or the
    // size of the table behind it - and never how many rows survived the
    // query, so a short list reads as filtered rather than as the whole set.
    let (icon, title, cap, rows) = match trigger {
        Trigger::Command => {
            let commands = home.surface.slash_commands(slot);
            let count = commands.len();
            let rows: Vec<Markup> = commands
                .iter()
                .filter(|command| matches_query(&[&command.name, &command.description], query))
                .take(CANDIDATES)
                .map(|command| row(&command.name, &command.description, None, query))
                .collect();
            ("cmd", "commands".to_owned(), count.to_string(), rows)
        }
        Trigger::File => {
            let index = match roster.cwd_for(slot) {
                Some(cwd) => home.work.files(slot, &cwd).await,
                None => std::sync::Arc::default(),
            };
            let found = index.visible(query, FILE_ROWS);
            let rows: Vec<Markup> =
                found.iter().map(|file| row(&file.rel_path, "", None, query)).collect();
            ("file", "files & folders".to_owned(), format!("{FILE_ROWS} max"), rows)
        }
        Trigger::Agent => {
            let agents = home.surface.subagents(slot);
            let rows: Vec<Markup> = agents
                .iter()
                .filter(|agent| matches_query(&[&agent.name, &agent.description], query))
                .take(AGENT_ROWS)
                .map(|agent| row(&agent.name, &agent.description, None, query))
                .collect();
            ("bot", "subagents".to_owned(), format!("{AGENT_ROWS} max"), rows)
        }
        Trigger::Emoji => {
            let found = forge_sessions::surface::ViewSurface::emoji(query, CANDIDATES);
            let rows: Vec<Markup> = found
                .iter()
                .map(|emoji| {
                    row(&format!(":{}:", emoji.name), "", Some(emoji.glyph.to_owned()), query)
                })
                .collect();
            let count = forge_sessions::emoji::count();
            ("smile", "emoji".to_owned(), format!("{count} shortcodes"), rows)
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
                @for (index, row) in rows.into_iter().enumerate() {
                    div class=(if index == 0 { "it sel" } else { "it" }) {
                        span .cur { @if index == 0 { "\u{25b8}" } }
                        (row)
                    }
                }
            }
            // The list is drawn, not yet choosable, and it is the one place
            // in the composer that could leave that unsaid: a selected row
            // advertises a key that nothing here reads.
            div .keys { span .off { "choosing a row is " (NO_DISPATCH) } }
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
/// What it offers came off the wire once and is not retained anywhere, so
/// it is `None` for a view that attached after the prompt landed - and that
/// case says so rather than drawing a box that would read as nothing
/// pending.
fn dock(row: Option<&AgentRow>, kind: PendingKind, ask: Option<&Ask>, endpoint: &str) -> Markup {
    html! {
        div .dock {
            @if let Some(depth) = row.map(|row| row.pending_depth).filter(|depth| *depth > 1) {
                div .queue { "\u{25bc} " (depth - 1) " more pending" }
            }
            @match ask {
                Some(Ask::Permission(request)) => (permission_dock(request, endpoint)),
                Some(Ask::Question(request)) => (question_dock(request, endpoint)),
                None => (unknown_dock(kind)),
            }
        }
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
                button .lbl type="submit" hx-post=(format!("{endpoint}/answer"))
                    hx-include="closest .dock" hx-vals=(format!(r#"{{"tool_id":"{tool_id}"}}"#))
                    hx-target="#comp" hx-swap="outerHTML" {
                    "Tell Claude something else:"
                }
            }
        }
        textarea .notes name="notes" rows="1"
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

// ---------- what the stream told this view ----------

/// The composer's own memory of the stream: the states a box can only be
/// drawn from because the core announced them once and kept none of them.
///
/// The core's pending set is what says a prompt waits; this holds only what
/// it offers. A take is here for the same reason, and its generation is what
/// keeps an older take's reports from drawing over a newer one.
#[derive(Default, Clone)]
pub struct Composer {
    takes: HashMap<SessionSlot, Take>,
    notices: HashMap<SessionSlot, Notice>,
    compacting: HashSet<SessionSlot>,
    asks: HashMap<SessionSlot, Ask>,
    sign_ins: HashMap<SessionSlot, SignIn>,
}

/// The sign-in a seat is waiting on, as the wire names it. The method is what
/// makes the hint say which account rather than only that one is needed.
#[derive(Clone)]
struct SignIn {
    method_name: String,
    method_description: String,
}

impl Composer {
    /// Fold one update in, answering whether a composer has to be redrawn.
    ///
    /// An update about a take this view is no longer drawing is dropped
    /// rather than drawn over the newer one, and drops answer false: a stale
    /// take's tail is not news.
    pub fn apply(&mut self, update: &SessionUpdate) -> bool {
        match update {
            SessionUpdate::DictateStarted { key, floor_db, generation } => {
                // A new take supersedes whatever the seat was doing, its
                // notice included: the words it left are already in the
                // draft the browser holds.
                self.takes.insert(key.clone(), Take::new(*floor_db, *generation));
                self.notices.remove(key);
                true
            }
            SessionUpdate::DictateLevel { key, peak_db } => {
                // The wire carries no generation on a level, and the
                // stream is one order per seat, so the take it belongs to
                // is whichever is live: the level that arrives after one
                // ended finds none and is dropped.
                if let Some(take) = self.takes.get_mut(key) {
                    take.push(*peak_db);
                    return true;
                }
                false
            }
            SessionUpdate::DictateTranscribing { key } => {
                if let Some(take) = self.takes.get_mut(key) {
                    take.phase = Phase::Transcribing;
                    return true;
                }
                false
            }
            SessionUpdate::DictateProgress { key, generation, done, total } => {
                if let Some(take) = self.takes.get_mut(key)
                    && take.generation == *generation
                {
                    take.progress = (*done, *total);
                    return true;
                }
                false
            }
            SessionUpdate::DictateEnded { key, outcome, generation } => {
                // A resolver for a take the composer has already replaced
                // is about a take that is over.
                if self.takes.get(key).is_none_or(|take| take.generation != *generation) {
                    return false;
                }
                let floor_db = self.takes.remove(key).map_or(-50.0, |take| take.floor_db);
                if let Some(notice) = Notice::of(outcome, floor_db) {
                    self.notices.insert(key.clone(), notice);
                }
                true
            }
            SessionUpdate::AuthRequired { key, method_name, method_description } => {
                self.sign_ins.insert(
                    key.clone(),
                    SignIn {
                        method_name: method_name.clone(),
                        method_description: method_description.clone(),
                    },
                );
                true
            }
            SessionUpdate::PermissionRequest { key, request, .. } => {
                self.asks.insert(key.clone(), Ask::Permission(Box::new(request.clone())));
                true
            }
            SessionUpdate::QuestionRequest { key, request, .. } => {
                self.asks.insert(key.clone(), Ask::Question(Box::new(request.clone())));
                true
            }
            // The prompt is settled, so the dock goes. This is the only
            // thing on the stream that says so: answering leaves the core's
            // pending set either way, and a view that answered from another
            // seat's page would otherwise keep drawing it.
            SessionUpdate::PendingInteractionResolved { key, tool_id } => {
                let held = match self.asks.get(key) {
                    Some(Ask::Permission(request)) => &request.tool_call.tool_call_id == tool_id,
                    Some(Ask::Question(request)) => &request.tool_call.tool_call_id == tool_id,
                    None => false,
                };
                if held {
                    self.asks.remove(key);
                }
                // True whatever this view held: the prompt is gone from the
                // core, and a view that never had the ask still draws the
                // dock from the core's own record of what is pending.
                true
            }
            // The CLI announces a compaction on the status frame and
            // clears it with a null, which is the only place either is
            // said. Everything else on the conversation is the chat's.
            SessionUpdate::ChatAppended { key, msg } => {
                if let Message::System { subtype, data, .. } = msg
                    && subtype == "status"
                {
                    let field = data.get("status");
                    if field.and_then(serde_json::Value::as_str) == Some("compacting") {
                        return self.compacting.insert(key.clone());
                    }
                    if field.is_some_and(serde_json::Value::is_null) {
                        return self.compacting.remove(key);
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn take(&self, slot: &SessionSlot) -> Option<&Take> {
        self.takes.get(slot)
    }

    fn notice(&self, slot: &SessionSlot) -> Option<&Notice> {
        self.notices.get(slot)
    }

    fn ask(&self, slot: &SessionSlot) -> Option<&Ask> {
        self.asks.get(slot)
    }

    fn compacting(&self, slot: &SessionSlot) -> bool {
        self.compacting.contains(slot)
    }

    fn sign_in(&self, slot: &SessionSlot) -> Option<&SignIn> {
        self.sign_ins.get(slot)
    }
}
