//! A subject's wire form, and the one place it is decided.
//!
//! The server hands a view the surface's own values, and a client reads a
//! shape built for it here rather than the core's structs. A twin per
//! subject keeps a wire change in one file instead of in a handler and a
//! test, and the fixtures beside this module are what make a changed shape
//! fail a test rather than a client.
//!
//! **The twins are complete.** A field is carried unless it is a Rust
//! mechanism rather than data - a `oneshot`, an `Arc<Workspace>` - because
//! the client is not today's pages: it is built against this wire and
//! compared with the terminal, and a field missing here cannot be added
//! later without a server change.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use forge_primitives::review::{ReviewSet, ReviewThread};
use forge_primitives::runtime::{AvailableAgent, AvailableCommand, MonitorRecord};
use forge_primitives::slack::SlackSubscription;
use forge_primitives::{ContentBlock, GotifySubscription, Message, SessionSlot, UserEnvelope};
use forge_workspace::env::git_diff::content::{DiffContent, scan as scan_content};
use forge_workspace::env::processes::ProcessSnapshot;
use forge_workspace::{
    AccountLoadingRow, Command, GatewayOrgView, McpServers, ProjectView, WorkerEntry,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::composer::SignIn;
use crate::file_index::FileIndex;
use crate::surface::{AgentRow, PendingAsk, ViewSurface};
use crate::transcript::{Rendered, TaskEnding, TurnSpan};
use crate::transport::TransportState;
use crate::transport::conversation::Held;
use crate::transport::envelope::Subject;
use crate::work::{WorkState, git_work_view, work_from_scan};

/// The seat the fixture surface is populated for.
///
/// One seat is enough: a fixture pins the SHAPE of a snapshot, and a second
/// seat would pin the same shape twice.
pub fn fixture_seat() -> SessionSlot {
    SessionSlot::lead("TestOrg", "proj")
}

/// One of the surface's two independent review reads: the records, or why
/// they could not be read. They fail apart, so neither is collapsed into
/// the other, and a client can tell an empty branch from an unreadable one.
#[derive(Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum ReadWire<T> {
    Read(T),
    Failed(String),
}

impl<T> From<std::result::Result<T, String>> for ReadWire<T> {
    fn from(read: std::result::Result<T, String>) -> Self {
        match read {
            Ok(records) => Self::Read(records),
            Err(why) => Self::Failed(why),
        }
    }
}

/// The home, as a client sees it: every read a home-scoped view makes.
///
/// The last two are the App-level state that reaches a home subscriber as
/// an update and nothing else - a subject's snapshot has to cover every
/// update that subject receives, or a late subscriber sees a smaller world
/// than one that attached at boot.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HomeWire {
    pub projects: Vec<ProjectWire>,
    pub agents: Vec<AgentWire>,
    /// The seats whose last turn finished while no client was showing them.
    ///
    /// The terminal draws a mark per row from this, and it is the fact a
    /// viewer cannot reconstruct: a turn that ended before a client attached
    /// leaves nothing in the transcript to say it went unwatched, so a client
    /// reading only the records would draw every row as settled.
    pub unseen: Vec<SessionSlot>,
    pub accounts: AccountsWire,
    pub plugins: PluginsWire,
    pub workers: Vec<WorkersWire>,
    pub connectors: ConnectorsWire,
    pub dictate: DictateWire,
    /// The claude CLI versions the core holds, as the core's own snapshot
    /// serialises. Its crate is one this one may not name.
    pub cli_version: Option<Value>,
    /// The forge build serving this socket: the full stamp and the short one.
    ///
    /// **A client draws these rather than its own package version.** The
    /// header states which forge commit is RUNNING, and the client is a
    /// different program with a version of its own - so a client rendering
    /// `env!("CARGO_PKG_VERSION")` would name itself in a header about forge.
    pub forge_version: String,
    pub forge_version_short: String,
    pub service_status: Option<Value>,
    /// The last fatal error, held by the core: an App-level event with no
    /// state behind it reaches only whoever was subscribed when it fired.
    pub fatal_error: Option<Value>,
}

/// One project row: the project, and the per-row reads the home draws it from.
///
/// The three beside the project are what the row's cells state, and each is
/// reached from the ROW rather than from a seat: a home row is a project, and
/// a project is a row whether or not anything has started it.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProjectWire {
    pub project: ProjectView,
    /// The branch and the count for the project's own tree.
    ///
    /// **Keyed by the project's LEAD slot, which exists for every declared
    /// project** whether or not a session has opened it, and read at the
    /// project's own path - so a row's cell is filled for a project nobody
    /// has started, which is the whole point of the cell. That is also why it
    /// is not the read a SEAT draws: a worker's own tree is `AgentWire::work`,
    /// and this one is the project's own path whichever seat is asking.
    ///
    /// This is the shared cache the transport holds, so several clients
    /// subscribing do not multiply git invocations: the cache is what makes
    /// the per-row cost the terminal's rather than N times it.
    pub work: WorkState,
    pub tasks: Vec<forge_primitives::tasks::Task>,
    /// The schedules this project holds.
    ///
    /// The terminal draws its SCHEDULES section from these, and nothing on
    /// this wire carried them: a client could create a cron and never see it
    /// again.
    pub crons: Vec<forge_primitives::CronEntry>,
    /// What this project's sessions are subscribed to, one list per connector.
    ///
    /// The subscriptions are per PROJECT and the section that draws them sits
    /// on the row, so they ride here rather than on the home's
    /// [`ConnectorsWire`] - which carries the liveness facts, which belong to
    /// the server and the workspace rather than to a project.
    pub connectors: ProjectConnectorsWire,
    /// Whether a spawn in this project would find an account. Read beside
    /// `project.has_model`, which is what tells the two reasons a spawn cannot
    /// run apart.
    pub would_bind: bool,
    /// The account this project's row chips, and its state.
    ///
    /// The terminal draws the chip from `Roster::chip_for`, which derives the
    /// state from the project's model and the account pool. `has_model` is not
    /// enough to derive it from, so a client could draw the row and not the
    /// chip it carries.
    pub chip: Option<forge_workspace::SessionChipInfo>,
}

/// One seat's row: the surface's own [`AgentRow`], plus the read that is per
/// SEAT rather than per project.
///
/// A home row is a seat, and its `where` cell is that seat's own working tree.
/// `ProjectWire::work` cannot answer it - that read is the project's own path
/// from the lead's seat - so a worker row drew the project's branch, and on a
/// fleet with workers in worktrees that is eight rows reading `main`.
///
/// The fields are spelled out rather than the row being carried whole, so a
/// field added to [`AgentRow`] has to be carried here deliberately rather than
/// reaching a client by accident.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentWire {
    pub slot: SessionSlot,
    pub label: String,
    pub lifecycle: forge_primitives::SessionLifecycleState,
    pub has_background_work: bool,
    pub pending: Option<forge_workspace::PendingInteractionKind>,
    pub pending_depth: usize,
    pub last_activity: Option<std::time::SystemTime>,
    pub reason: Option<String>,
    /// When the seat's newest turn ended in failure, filtered by what has
    /// been shown: `None` once the seat has been shown since the failure,
    /// or while it is being shown now.
    pub failed_turn: Option<std::time::SystemTime>,
    /// The seat's own tree, `None` for a seat forge holds no directory for.
    ///
    /// Read through the same shared cache the project rows use, so a lead's
    /// tree is one read drawn twice rather than two reads.
    pub work: Option<WorkState>,
}

impl From<&AgentRow> for AgentWire {
    fn from(row: &AgentRow) -> Self {
        Self {
            slot: row.slot.clone(),
            label: row.label.clone(),
            lifecycle: row.lifecycle,
            has_background_work: row.has_background_work,
            pending: row.pending,
            pending_depth: row.pending_depth,
            last_activity: row.last_activity,
            reason: row.reason.clone(),
            failed_turn: row.failed_turn,
            work: None,
        }
    }
}

/// The plugin inventory and its update records.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginsWire {
    pub update_records: Vec<Value>,
}

/// One project's live workers, keyed by the project they belong to.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkersWire {
    pub project: String,
    pub workers: Vec<WorkerWire>,
}

/// One live worker, minus the handles a client can never use.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerWire {
    pub label: String,
    pub charter: String,
    pub slot: SessionSlot,
    pub session_id: Option<forge_primitives::SessionId>,
    pub spawned_at: std::time::SystemTime,
    pub spawned_by: SessionSlot,
    pub needs_tag: bool,
    pub is_git_repo_at_spawn: bool,
}

impl From<&WorkerEntry> for WorkerWire {
    fn from(entry: &WorkerEntry) -> Self {
        Self {
            label: entry.label.clone(),
            charter: entry.charter.clone(),
            slot: entry.slot.clone(),
            session_id: entry.session_id.clone(),
            spawned_at: entry.spawned_at,
            spawned_by: entry.spawned_by.clone(),
            needs_tag: entry.needs_tag,
            is_git_repo_at_spawn: entry.is_git_repo_at_spawn,
        }
    }
}

/// The account pool's state.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AccountsWire {
    pub loading: Vec<AccountLoadingRow>,
    pub all_loaded: bool,
    pub gateway: GatewayWire,
    pub usage: Vec<AccountUsageWire>,
    pub orgs: Vec<GatewayOrgView>,
}

/// The inference listener's bind state.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GatewayWire {
    pub ready: bool,
    pub port: u16,
    pub bind_error: Option<String>,
}

/// One account's cached usage snapshot, absent until the poller succeeds.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AccountUsageWire {
    pub display_name: String,
    pub snapshot: Option<Value>,
}

/// What the inbound connectors are watching, server-wide.
///
/// The liveness facts and nothing else: what each connector is subscribed to
/// belongs to a project and rides [`ProjectWire::connectors`], so a
/// subscriptions field here would be one no read can fill - and a page drawing
/// it shows the section empty however long it waits.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConnectorsWire {
    pub gotify: GotifyWire,
    pub slack: SlackWire,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GotifyWire {
    pub connected: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SlackWire {
    /// Workspace label to whether that workspace's pump is live, as a list
    /// of pairs: JSON objects with dynamic keys are awkward for a client to
    /// iterate, and the order is the store's.
    pub connected_workspaces: Vec<(String, bool)>,
    pub load_failed: bool,
}

/// One project's connector subscriptions, as the row's section draws them.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProjectConnectorsWire {
    pub gotify: Vec<GotifySubscription>,
    pub slack: Vec<SlackSubscription>,
}

/// Dictation's preflight state.
///
/// **The device catalog is not a field here, and asking for it is its own
/// read.** Enumerating devices is a blocking walk on the machine running
/// forge, which trips a microphone check, and a record is encoded per request
/// and re-sent on every reconnect - so a field would be a permission check per
/// frame and per connection. What crosses here is the input a pick has already
/// moved this process to, in `device` below; the list to pick FROM is asked
/// for on demand and answered by the socket's `devices` message.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DictateWire {
    /// Whether `[dictate] enabled` is set. **Carried rather than inferred
    /// from an empty `models` list**: the list is empty both for a
    /// switched-off section and for a default snapshot, so a reader asserting
    /// the cause from the value tells a healthy configuration it is off, and
    /// nothing in the payload says which.
    pub enabled: bool,
    pub snapshot: Value,
    pub models_dir: Option<std::path::PathBuf>,
    /// The input a pick has moved this process to, over the configured pin.
    /// `None` means the pin stands.
    pub device: Option<forge_workspace::DictateDeviceChoice>,
}

/// One input forge can record from, as a picker draws it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DeviceWire {
    /// The stable identity, which is what a pick sends back: names collide
    /// between two identical interfaces and change when a user renames one.
    pub id: String,
    /// Human label for a picker. Not an identity.
    pub name: String,
    /// Whether the system would pick this one when asked for no particular
    /// device.
    pub is_default: bool,
}

/// One session, as a client sees it: every read a session-scoped view makes.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionWire {
    pub slot: SessionSlot,
    pub state: SessionStateWire,
    /// What this seat's composer is doing, which no record answered before.
    pub composer: ComposerWire,
    pub header: SessionHeaderWire,
    pub mcp: Option<McpServers>,
    pub processes: Option<ProcessSnapshot>,
    /// The CLI's background-task registry, which the processes feed leads its
    /// rows with.
    ///
    /// The tasks the OS walk cannot see for itself are the ones this exists
    /// for: a backgrounded bash is `setsid`-detached, so it sits outside
    /// claude's tree, and a view draws it from the registry - with the
    /// command telling it whether the walk has already adopted the process
    /// behind one.
    pub background_tasks: Vec<forge_workspace::BackgroundTask>,
    pub monitors: Vec<MonitorRecord>,
    /// The prompt a one-ask dock draws: the front of `pending_asks`.
    ///
    /// **Derived from the same list at serialisation**, so the two cannot
    /// drift. What a client reading only this draws is the front, which on a
    /// mixed set is NOT what the single read answered before the list: that
    /// preferred a question over a permission prompt and took same-kind asks
    /// in map order, where the front is the oldest ask with a draft leading.
    pub pending_ask: Option<PendingAskWire>,
    /// Every prompt this seat is holding: **a draft leads, then arrival
    /// order**. A parallel batch parks several at once, so a client that
    /// attached mid-batch reads the ones behind the front rather than losing
    /// all but one.
    pub pending_asks: Vec<PendingAskWire>,
    /// The newest turns, not the whole conversation. See [`SUBSCRIBE_TURNS`].
    pub conversation: ConversationWire,
    /// Whether this seat's conversation holds a sub-agent dispatch at all.
    ///
    /// On the record rather than left to whoever draws: it is a fact about
    /// the conversation, and `conversation` above carries a window of it, so
    /// a reader scanning that window would report a seat that dispatched an
    /// hour ago as one where nothing ever ran.
    pub has_dispatches: bool,
    pub slash_commands: Vec<AvailableCommand>,
    pub subagents: Vec<AvailableAgent>,
    /// The session's sub-agent instances, as the core joined them: one card
    /// per dispatch with its calls under it. The catalogue above names the
    /// TYPES the CLI offers; this is what actually ran.
    pub subagent_instances: Vec<forge_primitives::runtime::SubagentCard>,
    /// The seat's own walk, from the store its loop keeps fresh, or a walk
    /// taken for the read when the loop has not run. Shared rather than walked
    /// per subscriber: the walk is a whole tree. Serialises as the index
    /// itself.
    pub file_index: Arc<FileIndex>,
    pub reviews: ReviewsWire,
    /// The working tree behind the git section: the branch, how much has
    /// changed, and the gate. The changed files themselves ride `diff`
    /// below.
    pub work: WorkState,
    /// The tree behind the git row's depth: the uncommitted files with
    /// their marks and counts, the branch's chain ahead of its default
    /// with the commits that produced it, and which branch that is.
    ///
    /// What the row above cannot say: a branch and a count tell a reader
    /// nothing about WHAT is working, and this is what a hover over the
    /// row draws. The content read below carries the same layers with
    /// hunks, for the surface that reviews them.
    pub git: forge_primitives::git_diff::GitWorkView,
    /// The open pull request this seat's branch is on, and the issues it
    /// closes.
    ///
    /// Beside `work` rather than inside it because the two come from
    /// different reads - the branch and the count are the row's own, and this
    /// is the heavier scan - and a client draws the inspector's GIT section
    /// from both. Nothing carried them, so a client would draw the section
    /// without the `PR #N -> closes #M` row the terminal leads it with.
    pub pr: Option<forge_primitives::git::GitPrInfo>,
    pub closes: Vec<forge_primitives::git::GitIssueRef>,
    /// The changed files with their raw hunks, bounded and flagged: what a
    /// review surface draws a branch from, as data and never as a
    /// rendering.
    ///
    /// Read beside `work` rather than pushed with it: the row above moves
    /// with the tree, while this is taken once per read that encodes a
    /// record - a cold load, a reconnect, a seat swap - the way the
    /// terminal's `/diff` scans once on open. Its caps and flags live in
    /// forge-agent's `git_diff::content`, which is where the read's shape
    /// is decided.
    pub diff: DiffContent,
}

/// Where a session's reads find their own working tree.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionStateWire {
    pub slot: SessionSlot,
    pub scan_cwd: std::path::PathBuf,
    /// What this session dictates with, where it has overridden the defaults.
    pub dictate_overrides: forge_workspace::DictateOverrides,
    /// The prompts still waiting in the CLI's queue, oldest first.
    ///
    /// The read path for the pile: `prompt_queued` and `prompt_lifecycle`
    /// speak only on a change, so a client attaching mid-queue - a fresh
    /// load, a refresh, a seat switch - has nothing else to draw the waiting
    /// prompts from, and two attached clients have to agree about them.
    pub queue: Vec<forge_workspace::protocol::QueuedPrompt>,
}

/// What one seat's composer is doing.
///
/// A client attaching to a running session cannot reconstruct any of it: the
/// stream announces a compaction and a sign-in once each and retains nothing.
///
/// **No take and no notice**: a take belongs to the connection that started
/// it, whose own updates carry its meter, its phases and its words, so the
/// shared record says nothing about one and no other subscriber draws it.
///
/// The ASKS the composer is answering ride `pending_asks` on this same record
/// rather than appearing here, because that is the same thing the session's own
/// read answers and two copies of it would drift.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ComposerWire {
    pub compacting: bool,
    pub sign_in: Option<SignIn>,
    /// The push-to-talk key and how a press maps onto a take, in the
    /// vocabulary `forge.toml` accepts.
    ///
    /// **Carried rather than left to each client's default.** What a
    /// keyboard press means is the user's own configuration, and a client
    /// that drew the affordance from a hardcoded key would honour a
    /// different chord on every install that moved it.
    pub bind: String,
    pub mode: String,
}

/// The session's header facts, including the two a client cannot otherwise
/// reach: the catalogue a picker draws its rows from, and whether a turn is
/// in flight.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionHeaderWire {
    /// The occupant's id, or `None` when there is no occupant to name:
    /// nothing started, nothing connected yet, or an id dropped since.
    pub session_id: Option<forge_primitives::SessionId>,
    pub model: Option<Value>,
    pub effort: Value,
    pub permission_mode: Option<Value>,
    pub context: Value,
    pub available_models: Vec<Value>,
    pub turn_in_flight: bool,
}

/// What a seat is held on: every kind of parked interaction, not the two the
/// dock happened to draw first. Each request crosses as the core's own shape,
/// because a client draws it rather than re-deriving it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum PendingAskWire {
    Permission(Value),
    Question(Value),
    SlackDraft(Value),
    BrowserHandOff(Value),
}

impl From<&PendingAsk> for PendingAskWire {
    fn from(ask: &PendingAsk) -> Self {
        let encode = |body: Result<Value, serde_json::Error>| body.unwrap_or(Value::Null);
        match ask {
            PendingAsk::Permission(request) => {
                Self::Permission(encode(serde_json::to_value(request.as_ref())))
            }
            PendingAsk::Question(request) => {
                Self::Question(encode(serde_json::to_value(request.as_ref())))
            }
            PendingAsk::SlackDraft(draft) => {
                Self::SlackDraft(encode(serde_json::to_value(draft.as_ref())))
            }
            PendingAsk::BrowserHandOff(handoff) => {
                Self::BrowserHandOff(encode(serde_json::to_value(handoff.as_ref())))
            }
        }
    }
}

/// The conversation a session's transcript replayed, and how many times it
/// has compacted.
///
/// **Its turns, not a bare frame list.** A client folds turns, so a
/// conversation handed over as frames with no boundaries is one it cannot fold
/// at all; carrying the same turns a page carries is what makes the whole
/// conversation and a window of it read the same way.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConversationWire {
    pub turns: Vec<TurnWire>,
    pub compaction_count: u32,
}

/// The review reads for one branch, each carrying its own answer.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReviewsWire {
    pub threads: ReadWire<Vec<ReviewThread>>,
    pub reviews: ReadWire<Vec<ReviewSet>>,
}

/// One turn, as a page carries it: the key the server named it by, and the
/// messages it ran as.
///
/// **The messages, not the units.** How a run of tool calls groups inside a
/// turn is a drawing decision, so it belongs to whoever draws; what crosses is
/// the turn's own boundary, and that stays here because the paging contract is
/// built on it - `more` asks for turns, the cursor is a turn, and a page that
/// split one would leave a client stitching half a turn to the other half.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TurnWire {
    /// `None` when nothing named the turn: the key comes from a `Result`
    /// frame, and a conversation read from a transcript carries none.
    pub key: Option<String>,
    /// The frames this turn ran as, which is the same shape the session
    /// record's `conversation` carries.
    pub messages: Vec<Value>,
}

/// One page of history: the newest turns, in conversation order, and the
/// handle that asks for the ones above them.
pub struct Page {
    pub turns: Vec<TurnWire>,
    /// `None` means there is nothing above this page, which is the one case
    /// a client stops asking.
    pub cursor: Option<String>,
}

/// Where each turn opens, one entry per turn, and what names it.
///
/// **One message opens one turn**, so a user row carrying two blocks does not
/// put two boundaries at one index. A cursor names a message and resolves to
/// the first match, so two entries sharing an index would leave a walk a page
/// of its own to hand back before it moved - an empty one, since both
/// boundaries are that message.
fn opens_of(spans: &[TurnSpan]) -> Vec<(usize, Option<&str>)> {
    let mut opens: Vec<(usize, Option<&str>)> = Vec::with_capacity(spans.len());
    for span in spans {
        match opens.last_mut() {
            Some((at, key)) if *at == span.opens_at => {
                if key.is_none() {
                    *key = span.key.as_deref();
                }
            }
            _ => opens.push((span.opens_at, span.key.as_deref())),
        }
    }
    opens
}

/// The messages `from..to` as one turn, which is the shape both the session
/// record and a page carry.
///
/// **A task ending a call in this turn is owed rides along.** A client folds
/// one turn at a time, so an ending the CLI persisted in another turn is one
/// it cannot reach - and the notice opens a turn of its own, which puts it in
/// another turn whenever anything that does open one sits between. The row
/// the CLI wrote stays where it is and draws nothing; what is added is a frame
/// of its own, which is content of the wire's own type rather than a second
/// copy of a drawing.
fn turn_wire(
    messages: &[Message],
    from: usize,
    to: usize,
    key: Option<&str>,
    endings: &HashMap<String, TaskEnding>,
) -> TurnWire {
    let mut frames: Vec<Value> = messages[from..to]
        .iter()
        .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
        .collect();
    for ending in owed_endings(messages, from, to, endings) {
        frames.push(serde_json::to_value(forged_ending(ending)).unwrap_or(Value::Null));
    }
    TurnWire { key: key.map(str::to_owned), messages: frames }
}

/// The endings that name a call this turn holds and that the turn does not
/// already carry.
fn owed_endings<'a>(
    messages: &[Message],
    from: usize,
    to: usize,
    endings: &'a HashMap<String, TaskEnding>,
) -> Vec<&'a TaskEnding> {
    endings
        .values()
        .filter(|ending| !(from..to).contains(&ending.at))
        .filter(|ending| turn_holds_call(&messages[from..to], &ending.call))
        .collect()
}

/// Whether these frames make the call `id`.
fn turn_holds_call(messages: &[Message], id: &str) -> bool {
    messages.iter().any(|message| {
        let Message::Assistant { message: envelope, .. } = message else {
            return false;
        };
        envelope.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolUse { id: call, .. }
                    | ContentBlock::ServerToolUse { id: call, .. }
                    if call == id
            )
        })
    })
}

/// The notice as a frame of its own, for the turn that holds the call it ends.
///
/// Forged rather than read off the wire, like the turn a delivery draws as:
/// the CLI wrote this ending into a row of its own, and this is that ending
/// delivered where a per-turn fold can see it. It carries no `uuid`, because
/// the CLI mints the transcript's id for its own row and nothing here may
/// invent one that disagrees with it.
fn forged_ending(ending: &TaskEnding) -> Message {
    Message::User {
        message: UserEnvelope {
            role: "user".to_owned(),
            content: vec![crate::transcript::notice_block(&ending.text)],
            extras: serde_json::Map::new(),
        },
        session_id: String::new(),
        parent_tool_use_id: None,
        uuid: None,
        tool_use_result: None,
        timestamp: None,
        synthetic: false,
        extras: serde_json::Map::new(),
    }
}

/// The message range of every turn `spans` names, in conversation order, each
/// with what names it.
///
/// **The one place a turn's boundaries are computed.** The session record
/// carries every turn and a page carries a window of them, and a second
/// computation is how the two would come to disagree about what a turn is.
/// Ranges rather than turns, so a page renders the window it keeps instead of
/// every turn the conversation holds.
fn turn_ranges<'a>(
    messages: &[Message],
    spans: &'a [TurnSpan],
) -> Vec<(usize, usize, Option<&'a str>)> {
    let opens = opens_of(spans);
    // **A conversation can hold no turn at all**, and a client folds turns, so
    // it rides one instead of none. A session a cron fired into that nobody
    // typed into is user rows that draw as notices, and a delivery-only one is
    // the same; handing a client no turns would leave the whole of it
    // unreachable, which is exactly what a page's `None` cursor stops it asking
    // for.
    if opens.is_empty() {
        return if messages.is_empty() { Vec::new() } else { vec![(0, messages.len(), None)] };
    }
    // The conversation's opening rows - who started it, a cron fire, a delivery
    // - come before its first turn, so the first turn carries them. Leaving
    // them above it would put them where no walk reaches.
    (0..opens.len())
        .map(|at| {
            let from = if at == 0 { 0 } else { opens[at].0 };
            let to = opens.get(at + 1).map_or(messages.len(), |&(open, _)| open);
            (from, to, opens[at].1)
        })
        .collect()
}

/// `messages` as the whole turns `rendered` names, in conversation order.
///
/// The fold's own answer is what a caller passes, so the endings it read ride
/// along rather than being searched for again here.
pub fn all_turns(messages: &[Message], rendered: &Rendered) -> Vec<TurnWire> {
    turn_ranges(messages, &rendered.turns)
        .into_iter()
        .map(|(from, to, key)| turn_wire(messages, from, to, key, &rendered.endings))
        .collect()
}

/// Slice a conversation into whole turns, newest first.
///
/// **A turn is a RUN of the fold's units, and the fold is what says where one
/// begins** - the span `render` reports, in message terms. Slicing on a count
/// of messages instead would cut a turn in half, which is the breakage the
/// design exists to avoid.
///
/// **The cursor errs toward OVERLAP, never toward a gap.** A client asking
/// for more may be handed a turn it already has - it keys its turns and drops
/// the repeats - but never a HOLE, which is history it has no way to ask for
/// again.
///
/// **The cursor is a POSITION, not a name, and the reason is a fact about the
/// read rather than a preference.** A turn's name is its key, and the fold
/// builds one only from a `Message::Result` frame - while a conversation read
/// from a transcript carries none: `SessionMessageKind` is `User | Assistant |
/// System` with no Result kind, and the replay synthesizer never emits one. So
/// a keyed cursor is `None` on every page of every transcript-derived
/// conversation, and a client reading `None` as "nothing above" stops after
/// the first page. The message the page's first turn opens at is the one thing
/// that always names it.
///
/// **`dropped` is how many messages the held conversation has lost off the
/// front**, and it is what makes that position survive a drop: the held list
/// is renumbered by one and the conversation is not, so a page carries the
/// conversation's numbering and this is the offset between the two.
pub fn page(
    messages: &[Message],
    rendered: &Rendered,
    dropped: usize,
    before: Option<&str>,
    turns: u32,
) -> Page {
    // A page of no turns ends where it began: its cursor would name the turn it
    // already opened at, so a client walking back would ask for the same page
    // forever.
    let turns = turns.max(1) as usize;
    let ranges = turn_ranges(messages, &rendered.turns);
    let opens = opens_of(&rendered.turns);

    // A cursor names the message the previous page BEGAN at, so the page above
    // ends where that one started: the two meet exactly.
    //
    // **In the conversation's numbering rather than the held list's.**
    // `dropped` is what the cap has taken off the front: a cursor written
    // before a drop still names the message it meant, and one resolving below
    // the floor names a turn this seat has let go - answered from the empty
    // page above it rather than from a window the client did not ask for. A
    // cursor no turn opens at, or one this server did not write, keeps the
    // fallback it always had.
    let ends_at = match before.and_then(|cursor| cursor.parse::<usize>().ok()) {
        None => ranges.len(),
        Some(named) => match named.checked_sub(dropped) {
            None => 0,
            Some(started) => {
                opens.iter().position(|&(open, _)| open == started).unwrap_or(ranges.len())
            }
        },
    };

    // Only the window is rendered: building every turn to keep a few of them
    // would walk and encode the whole conversation on a path a reader hits
    // while scrolling.
    let first = ends_at.saturating_sub(turns);
    let page_turns: Vec<TurnWire> = ranges[first..ends_at]
        .iter()
        .map(|&(from, to, key)| turn_wire(messages, from, to, key, &rendered.endings))
        .collect();

    // `None` is the real "nothing above this page": a page already opening on
    // the conversation's first turn has nothing to walk back to, and that is
    // the one case a client stops asking.
    let cursor = if first == 0 {
        None
    } else {
        // Written back into the conversation's numbering, which is the
        // numbering the client hands it back in.
        opens.get(first).map(|&(open, _)| (open + dropped).to_string())
    };

    Page { turns: page_turns, cursor }
}

/// How long a request waits for the seat's conversation before answering
/// without one.
///
/// The session task answers a replay on its own next loop iteration, so this
/// is a failure case rather than a pacing mechanism. **What it decides is the
/// same for both readers and they answer differently**: a snapshot carries an
/// empty conversation, which a client draws as a seat with nothing to show,
/// while a page is REFUSED - an empty page carries `cursor: null`, and a
/// client reads that as the end of the history rather than as a delay.
const REPLAY_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// The seat's conversation, asking the session task for it when the stream
/// has not seeded one.
///
/// **The seed comes from the task that produces the conversation, never from
/// a second walk of the transcript.** The fold holds `Connected` for every
/// seat that starts or resumes while this transport runs; for a seat whose
/// connect it missed - a session already running when the transport started -
/// it asks the task, which is handed the history at connect and emits every
/// frame after it, so what it hands back is in one order with the frames
/// around it.
///
/// No lock is held across the wait, and two callers on two cold seats do not
/// serialise: each waits on its own seat's arrival, and the notice only says
/// that some seat was seeded.
pub async fn conversation_for(state: &TransportState, slot: &SessionSlot) -> Option<Arc<Held>> {
    if let Some(held) = state.conversations.get(slot) {
        return Some(held);
    }
    let deadline = std::time::Instant::now() + REPLAY_WAIT;
    // Asked BEFORE the loop's first look, so a seat that resolves between
    // the look and the wait is still seen on this pass rather than costing
    // the caller its whole budget.
    if state.surface.dispatch(Command::ReplayConversation { key: slot.clone() }).is_err() {
        return None;
    }
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let seeded = state.conversations.seeded_notice();
        tokio::pin!(seeded);
        seeded.as_mut().enable();
        if let Some(held) = state.conversations.get(slot) {
            return Some(held);
        }
        if tokio::time::timeout(left, seeded).await.is_err() {
            break;
        }
    }
    let held = state.conversations.get(slot);
    if held.is_none() {
        tracing::warn!(
            event_name = "conversation_replay_timed_out",
            slot = %slot.display(),
            waited_ms = REPLAY_WAIT.as_millis(),
            "no conversation for this seat within the wait; the request is answered \
             without one, which a client draws the same way it draws a seat that is \
             not running",
        );
    }
    held
}

/// How many of a conversation's newest turns a subscribe carries.
///
/// **A subscribe is not a request for a whole transcript.** A client draws a
/// window and asks `more` for what is above it, so handing it the whole
/// conversation is a transcript's worth of bytes written per client per
/// connect and per refresh - measured at 111.7 MB of socket for four
/// refreshes of a 68.9 MB transcript, which is 99% of that connection's
/// traffic - for turns it will not draw.
///
/// The window matches the client's own `MORE_TURNS` (20), so the newest page
/// a subscribe carries and the newest page a client asks for are the same
/// page: a client that asks anyway is handed what it already holds, which it
/// drops by turn key.
pub const SUBSCRIBE_TURNS: u32 = 20;

/// A subject's wire form. The ONE place it is produced.
///
/// Async because a session's git section is a filesystem read, so the
/// working tree's state is awaited rather than guessed at.
///
/// A seat naming no project forge has loaded is an error rather than an empty
/// snapshot, because a client drawing an empty page would read that as a
/// broken one rather than as a seat that does not exist.
///
/// **A project that has simply not started is not that case**: its directory
/// resolves from the declaration, so it is answered - with the session's own
/// fields empty and a working tree read per read, since no held seat has a
/// store to keep one in.
pub async fn encode_subject(state: &TransportState, subject: &Subject) -> Result<Value> {
    let surface = &state.surface;
    match subject {
        Subject::Home => Ok(serde_json::to_value(home(state, surface).await)?),
        Subject::Session(slot) => {
            let roster = surface.roster();
            let Some(cwd) = roster.cwd_for(slot) else {
                anyhow::bail!("forge holds no session for {}", slot.display());
            };
            // The seat's walk is not taken here: the connection holds the seat
            // before this encode, and the hold walks - so the snapshot below
            // is the one the hold just read, and the seat's loop keeps it
            // fresh for as long as somebody is showing it.
            request_context_usage_if_unreported(surface, slot);
            request_mcp_snapshot_if_unreported(surface, slot);
            Ok(serde_json::to_value(session(state, surface, slot, &cwd).await?)?)
        }
        // The pool's own report, scanned here rather than carried in another
        // snapshot: it belongs to no seat, and a home snapshot that scanned
        // the pool would pay for the walk on every subscribe.
        Subject::Usage => Ok(serde_json::to_value(surface.usage().await?)?),
        // The models page's read, off the workspace's own state: no walk,
        // no lock held across a scan - the fetch that changes it runs
        // elsewhere and lands as its own update.
        Subject::DictateModels => Ok(serde_json::to_value(surface.dictate_models())?),
    }
}

/// Ask the core for a context reading on `slot` when the seat reports none.
///
/// The reading exists only once the CLI has computed it, and the terminal asks
/// for the seat it is addressing. A client subscribing to a seat is that same
/// act, so the ask belongs on the read that encodes the subject: a seat only a
/// client watches would otherwise report nothing for the life of its session,
/// and the header would draw the dash an unasked seat draws rather than a bar.
///
/// Guarded on the reading rather than on the ask having happened, so a seat
/// that already reports one is not probed again by every subscribe, reconnect
/// and second tab. The answer arrives as
/// [`SessionUpdate::ContextUsageSnapshot`](crate::SessionUpdate::ContextUsageSnapshot)
/// on the stream the subscriber is already reading.
fn request_context_usage_if_unreported(surface: &ViewSurface, slot: &SessionSlot) {
    if surface.header(slot).context.percent.is_some() {
        return;
    }
    if let Err(error) = surface.refresh_context_usage(slot) {
        tracing::debug!(
            event_name = "context_usage_request_failed",
            %error,
            slot = %slot.display(),
            "a seat a client reads reports no context usage and its probe was not requested",
        );
    }
}

/// Ask the core for an MCP snapshot on `slot` when the seat reports none.
///
/// The snapshot exists only once the bridge has answered one, and only the
/// terminal's connect, poll and `/mcp` paths ask for it. A client subscribing
/// to a seat is that same act, so the ask belongs on the read that encodes the
/// subject: a seat only a client watches would otherwise report `mcp: None`
/// for the life of its session, and the strip's MCP row would never draw.
///
/// Guarded on the reading rather than on the ask having happened, so a seat
/// that already reports one - an empty set included, which is a snapshot
/// carrying none rather than no snapshot - is not probed again by every
/// subscribe, reconnect and second tab. The answer arrives as
/// [`SessionUpdate::McpSnapshot`](crate::SessionUpdate::McpSnapshot) on the
/// stream the subscriber is already reading. A seat replacement clears the
/// snapshot, which is the one re-ask this guard makes on its own: the field
/// reads `None` again and the next read asks.
fn request_mcp_snapshot_if_unreported(surface: &ViewSurface, slot: &SessionSlot) {
    if surface.mcp_servers(slot).is_some() {
        return;
    }
    if let Err(error) = surface.refresh_mcp_snapshot(slot) {
        tracing::debug!(
            event_name = "mcp_snapshot_request_failed",
            %error,
            slot = %slot.display(),
            "a seat a client reads reports no MCP snapshot and its ask was not requested",
        );
    }
}

/// The home's record, from the reads a home-scoped view makes.
///
/// Async because a row's work state is a filesystem read, cached by the
/// shared `WorkCache` the transport holds.
async fn home(state: &TransportState, surface: &ViewSurface) -> HomeWire {
    let roster = surface.roster();
    let accounts = surface.accounts();
    let agents = surface.agents();
    let connectors = surface.connectors(None);
    let dictate = surface.dictate();
    let workers = surface.workers();
    let encode = |value: Option<Value>| value;

    let mut projects = Vec::with_capacity(roster.projects.len());
    for project in &roster.projects {
        let seat = SessionSlot::lead(project.org.clone(), project.name.clone());
        let subscribed = surface.connectors(Some(&project.name));
        projects.push(ProjectWire {
            work: state.work.snapshot(&seat, &project.path).await,
            tasks: roster.tasks_for_project(&project.name),
            crons: roster.crons_for_project(&project.name),
            connectors: ProjectConnectorsWire {
                gotify: subscribed.gotify.subscriptions,
                slack: subscribed.slack.subscriptions,
            },
            would_bind: roster.would_bind(&project.key),
            chip: roster.chip_for(&project.key),
            project: project.clone(),
        });
    }

    // Each seat's own tree, read at the directory `cwd_for` resolves FOR IT:
    // the project's path for a lead, the worktree for a git worker. A seat
    // forge holds no directory for keeps `None` rather than borrowing the
    // project's read, which is what the cell's blank has to mean.
    //
    // The live snapshot filters the failure mark by what has been shown:
    // the same facts the diamond rides, read once for both.
    let live = crate::live::Live::lock(&state.live).snapshot();
    let mut agent_rows = Vec::with_capacity(agents.all().len());
    for agent in agents.all() {
        let work = match roster.cwd_for(&agent.slot) {
            Some(cwd) => Some(state.work.snapshot(&agent.slot, cwd.as_path()).await),
            None => None,
        };
        let mut row = AgentWire { work, ..AgentWire::from(agent) };
        row.failed_turn = row.failed_turn.and_then(|at| live.failed_mark(&agent.slot, at));
        agent_rows.push(row);
    }

    HomeWire {
        workers: roster
            .projects
            .iter()
            .map(|project| WorkersWire {
                project: project.name.clone(),
                workers: workers.for_project(&project.key).iter().map(WorkerWire::from).collect(),
            })
            .collect(),
        accounts: AccountsWire {
            loading: accounts.loading,
            all_loaded: accounts.all_loaded,
            gateway: GatewayWire {
                ready: accounts.gateway.ready,
                port: accounts.gateway.port,
                bind_error: accounts.gateway.bind_error,
            },
            usage: accounts
                .usage
                .into_iter()
                .map(|row| AccountUsageWire {
                    display_name: row.display_name,
                    snapshot: row.snapshot.and_then(|snapshot| serde_json::to_value(snapshot).ok()),
                })
                .collect(),
            orgs: accounts.orgs,
        },
        plugins: PluginsWire {
            update_records: surface
                .plugins()
                .update_records
                .iter()
                .filter_map(|record| serde_json::to_value(record).ok())
                .collect(),
        },
        connectors: ConnectorsWire {
            gotify: GotifyWire { connected: connectors.gotify.connected },
            slack: SlackWire {
                connected_workspaces: connectors.slack.connected_workspaces.into_iter().collect(),
                load_failed: connectors.slack.load_failed,
            },
        },
        dictate: DictateWire {
            enabled: dictate.enabled,
            snapshot: serde_json::to_value(dictate.snapshot).unwrap_or(Value::Null),
            models_dir: dictate.models_dir,
            device: dictate.device,
        },
        cli_version: surface.cli_version().and_then(|version| serde_json::to_value(version).ok()),
        forge_version: crate::FORGE_VERSION.to_owned(),
        forge_version_short: crate::FORGE_VERSION_SHORT.to_owned(),
        service_status: surface.service_status().and_then(|issue| serde_json::to_value(issue).ok()),
        fatal_error: encode(
            surface.fatal_error().and_then(|error| serde_json::to_value(error).ok()),
        ),
        agents: agent_rows,
        unseen: agents
            .all()
            .iter()
            .map(|row| row.slot.clone())
            .filter(|slot| live.unseen.is_unseen(slot))
            .collect(),
        projects,
    }
}

/// One session's record, from the reads a session-scoped view makes.
async fn session(
    state: &TransportState,
    surface: &ViewSurface,
    slot: &SessionSlot,
    cwd: &Path,
) -> Result<SessionWire> {
    let header = surface.header(slot);
    // The conversation's share of the record, taken through one blocking task
    // so the fold never runs on the reactor or under the lock. A seat with no
    // conversation is answered with the empty one.
    let conversation = conversation_for(state, slot).await;
    let (turns, compaction_count) = match conversation {
        Some(held) => {
            let seat = slot.clone();
            tokio::task::spawn_blocking(move || {
                held.read(|held| {
                    (
                        page(
                            held.messages(),
                            held.rendered(),
                            held.dropped(),
                            None,
                            SUBSCRIBE_TURNS,
                        )
                        .turns,
                        held.compaction_count(),
                    )
                })
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    event_name = "transcript_fold_failed",
                    %error,
                    slot = %seat.display(),
                    "the fold did not finish; the record is answered without it",
                );
                (Vec::new(), 0)
            })
        }
        None => (Vec::new(), 0),
    };
    // The instance list crosses from the fold that ANNOUNCES it rather than
    // from a second walk here: the session task folds a card per frame and
    // pushes `SubagentCardsChanged` off the same walk, so this is what that
    // fold holds. **It was folded here until the cap landed** - over the whole
    // held conversation on every read, which a drop would have shortened: a
    // record answering a window of cards over a window of messages is a
    // section that shrinks with the cap rather than with the conversation.
    let instances = surface.subagent_cards(slot);
    // The dispatch flag is the workspace's, raised by the fold that can
    // announce its raise: read here rather than recomputed, so the record and
    // the `DispatchesChanged` frame cannot disagree about it.
    let has_dispatches = surface.has_dispatches(slot);
    // The seat's scan, as the loop that owns it last answered it. Nothing is
    // read here: the tree is read for a seat somebody is showing, and this
    // answers what that read found - the tile's branch, count and PR are one
    // instant, so a branch switch cannot render a PR for a branch this record
    // does not name.
    let held = surface.work(slot, cwd).await;
    let work = work_from_scan(&held.diff, &held.cwd);
    // The tree behind the row's depth, from the same held scan and through
    // the same constructor the pushed frame goes through - one derivation,
    // so a client that applies the update lands on the record's own answer.
    let git = git_work_view(&held.diff);
    // The content read, beside the stats one: bounded, and taken here
    // rather than stored with the row because a record read is a cold load,
    // a reconnect or a seat swap - not a frame a moving tree pushes.
    let diff = scan_content(&held.cwd, &held.diff).await;
    let branch = work.branch.clone().unwrap_or_default();
    let reviews = surface.reviews(slot.project(), &branch);
    let state_at = surface.session(slot, cwd);
    // The seat's own loop walks the index and pushes the movement, so this
    // answers that store - and a seat nothing has walked yet is walked here,
    // the same fallback the work row above takes: a seat whose loop never ran
    // (no resolvable cwd, a read before the first poke) answers its tree
    // rather than an empty list the composer would draw nothing from. The walk
    // is a whole tree on the calling thread, so it runs off the reactor; one
    // that panicked reads as no files rather than as the page's problem.
    let file_index = if let Some(index) = surface.file_index(slot) {
        index
    } else {
        let walker = std::sync::Arc::clone(&state.surface);
        let root = cwd.to_owned();
        tokio::task::spawn_blocking(move || walker.walk_file_index(&root))
            .await
            .map(std::sync::Arc::new)
            .unwrap_or_default()
    };

    // One read for both halves of the record: the front is the first of the
    // list, so the two cannot disagree.
    let asks = surface.pending_asks(slot);

    Ok(SessionWire {
        slot: slot.clone(),
        file_index,
        slash_commands: surface.slash_commands(slot),
        subagents: surface.subagents(slot),
        subagent_instances: instances,
        mcp: surface.mcp_servers(slot),
        processes: surface.processes(slot),
        background_tasks: surface.background_tasks(slot),
        monitors: surface.monitors(slot),
        pending_ask: asks.first().map(PendingAskWire::from),
        pending_asks: asks.iter().map(PendingAskWire::from).collect(),
        conversation: ConversationWire { turns, compaction_count },
        has_dispatches,
        header: SessionHeaderWire {
            session_id: header.session_id.clone(),
            model: header.model.as_ref().and_then(|model| serde_json::to_value(model).ok()),
            effort: serde_json::to_value(header.effort).unwrap_or(Value::Null),
            permission_mode: header
                .permission_mode
                .as_ref()
                .and_then(|mode| serde_json::to_value(mode).ok()),
            context: serde_json::to_value(header.context).unwrap_or(Value::Null),
            available_models: header
                .available_models
                .iter()
                .filter_map(|model| serde_json::to_value(model).ok())
                .collect(),
            turn_in_flight: header.turn_in_flight,
        },
        reviews: ReviewsWire {
            threads: ReadWire::from(reviews.threads),
            reviews: ReadWire::from(reviews.reviews),
        },
        state: SessionStateWire {
            slot: state_at.slot,
            scan_cwd: state_at.scan_cwd,
            dictate_overrides: state_at.dictate_overrides,
            queue: state_at.queue,
        },
        composer: {
            let live = crate::live::Live::lock(&state.live).snapshot();
            let composer = &live.composer;
            ComposerWire {
                compacting: composer.compacting(slot),
                sign_in: composer.sign_in(slot).cloned(),
                bind: surface.dictate_bind().label().to_owned(),
                mode: surface.dictate_mode().label().to_owned(),
            }
        },
        work,
        git,
        pr: held.diff.pr,
        closes: held.diff.closes,
        diff,
    })
}

#[cfg(test)]
mod tests {
    /// The transcript rows one turn leaves: what the user wrote, what the
    /// assistant said, and the result that closes it. The result carries a
    /// `uuid`, which is what the fold names the turn by - so a turn's report
    /// has a key, and a key is what a page's cursor is.
    fn a_turns_rows(turn: usize) -> String {
        format!(
            r#"{{"type":"user","uuid":"u{turn}","message":{{"role":"user","content":"turn {turn}"}}}}
{{"type":"assistant","uuid":"a{turn}","message":{{"id":"m{turn}","role":"assistant","model":"claude-opus-5","content":[{{"type":"text","text":"reply {turn}"}}]}}}}
{{"type":"result","uuid":"r{turn}","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}}"#
        )
    }

    /// The messages and the fold's own answer for them, over a transcript of
    /// `rows`, read the way the transport reads them.
    fn a_conversation(rows: &[&str]) -> (Vec<Message>, Rendered) {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.seed_transcript("TestOrg", "proj", "lead", rows).expect("the transcript seeds");
        let seat = fixture_seat();
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");

        let messages = surface.conversation(&seat, &cwd).messages;
        let rendered = crate::transcript::render(&messages);
        (messages, rendered)
    }

    /// `turns` finished turns, as the transcript rows they are.
    fn turns_of(turns: usize) -> Vec<String> {
        (0..turns).map(a_turns_rows).collect()
    }

    /// The words a turn opened on, read off the messages the page carried.
    ///
    /// A turn's identity is its messages now, so this is what a reader checks
    /// one by: the evidence cannot be a position, because `page` hands back
    /// values and no value is ever `ptr::eq` to the original.
    fn opened_on(turn: &TurnWire) -> String {
        turn.messages
            .iter()
            .find_map(|frame| frame["message"]["content"][0]["text"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// The words every turn of a conversation opened on, oldest first.
    fn every_turn(messages: &[Message], rendered: &Rendered) -> Vec<String> {
        page(messages, rendered, 0, None, u32::MAX).turns.iter().map(opened_on).collect()
    }

    /// A turn's frames as the messages they are, so a test can fold one turn
    /// the way a client folds it: alone.
    fn turn_messages(turn: &TurnWire) -> Vec<Message> {
        turn.messages
            .iter()
            .map(|frame| serde_json::from_value(frame.clone()).expect("a message"))
            .collect()
    }

    /// How many task endings the frames name `call` in.
    fn endings_in(turn: &TurnWire, call: &str) -> usize {
        turn.messages
            .iter()
            .filter(|frame| {
                frame["message"]["content"].as_array().is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["type"] == "queued_command"
                            && block["commandMode"] == "task-notification"
                            && block["prompt"].as_str().is_some_and(|text| text.contains(call))
                    })
                })
            })
            .count()
    }

    /// The calls the fold drew in one turn.
    fn calls(units: &[ChatUnit]) -> Vec<&crate::transcript::ToolLeaf> {
        units
            .iter()
            .filter_map(|unit| match unit {
                ChatUnit::ToolGroup { families, .. } => {
                    Some(families.iter().flat_map(|family| family.calls.iter()))
                }
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// What the frames of one turn say about a call's row, in words.
    fn drawn_text(units: &[ChatUnit], call: &str) -> Vec<String> {
        calls(units)
            .into_iter()
            .filter(|leaf| leaf.id == call)
            .flat_map(|leaf| leaf.content.iter())
            .filter_map(|piece| match piece {
                forge_primitives::ToolCallContent::Content {
                    content: forge_primitives::ChunkContent::Text { text },
                } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// A page's client folds one turn at a time, so an ending that sits in
    /// another turn is an ending it cannot reach - and about three notices in
    /// ten on this machine sit exactly there.
    ///
    /// Both carriers are carried into the call's turn, and the ending is drawn
    /// exactly once: the copy is what the call's own turn holds, and the row
    /// the CLI persisted draws nothing wherever it is.
    #[test]
    fn the_ending_is_carried_into_the_call_turn() {
        for (rows, call, said) in [
            (
                crate::fixtures::SAME_TURN_USER_ROW,
                "call_da4c7ee117d14d48ba99e036",
                "Background command \"Watch the account-lifecycle CI run\" failed with exit code 1",
            ),
            (
                crate::fixtures::CROSS_TURN_ATTACHMENT,
                "call_17c05f64497746b0ac450728",
                "Background command \"Restart the harness with a long terminate window\" failed with exit code 100",
            ),
        ] {
            let (messages, rendered) = a_conversation(rows);
            let turns = all_turns(&messages, &rendered);
            // Folded the way a client folds a page: one turn at a time.
            let folded: Vec<Vec<ChatUnit>> = turns
                .iter()
                .map(|turn| crate::transcript::render_units(&turn_messages(turn)))
                .collect();

            let holding = folded
                .iter()
                .position(|units| calls(units).iter().any(|leaf| leaf.id == call))
                .unwrap_or_else(|| panic!("one turn holds {call}"));
            let held = calls(&folded[holding]);
            let held = held.iter().find(|leaf| leaf.id == call).expect("the call");

            assert_eq!(
                held.status,
                crate::model::ToolCallStatus::Failed,
                "the call's own turn carries the ending, so a per-turn fold reads it",
            );
            assert!(
                drawn_text(&folded[holding], call).iter().any(|text| text == said),
                "and what the harness said rides it",
            );
            // **The ending is drawn once.** A cross-turn notice is carried
            // as well as kept, so the conversation holds the ending twice -
            // the row the CLI wrote it in and the copy - and only one of them
            // can end the call: the copy is inside the turn that draws it,
            // and the row is in a turn whose fold finds no such call to end.
            let drawn_in: Vec<usize> = folded
                .iter()
                .enumerate()
                .filter(|(_, units)| {
                    calls(units).iter().any(|leaf| {
                        leaf.id == call && leaf.status == crate::model::ToolCallStatus::Failed
                    })
                })
                .map(|(at, _)| at)
                .collect();
            assert_eq!(drawn_in, vec![holding], "and it is drawn in that turn alone");
            assert!(
                turns.iter().any(|turn| endings_in(turn, call) > 0),
                "and it reaches the wire as the block a view reads, not as the row's own text",
            );
            assert!(
                folded.iter().flatten().all(|unit| !matches!(
                    unit,
                    ChatUnit::UserTurn { text } if text.contains("<task-notification>")
                )),
                "and no turn draws the raw notice as the reader's own words",
            );

            // **And the carry survives a page, which is the path a client
            // walking history actually reads.** `page` cuts its own window
            // from the same fold, and a carry that only `all_turns` made would
            // leave the ending unreachable where scrolling looks for it - a
            // hole no test above this line would show.
            let mut paged: Vec<Vec<ChatUnit>> = Vec::new();
            let mut before: Option<String> = None;
            loop {
                let page = page(&messages, &rendered, 0, before.as_deref(), 1);
                for turn in &page.turns {
                    paged.push(crate::transcript::render_units(&turn_messages(turn)));
                }
                match page.cursor {
                    Some(cursor) if Some(&cursor) != before.as_ref() => before = Some(cursor),
                    _ => break,
                }
            }
            let paged_drawn: usize = paged
                .iter()
                .filter(|units| {
                    calls(units).iter().any(|leaf| {
                        leaf.id == call && leaf.status == crate::model::ToolCallStatus::Failed
                    })
                })
                .count();
            assert_eq!(paged_drawn, 1, "a page of one turn, folded alone, still ends the call");
        }
    }

    /// A page of no turns ended where it began - an empty page whose cursor
    /// named the turn it had already opened at - so a client walking back
    /// asked for it forever.
    #[test]
    fn a_page_of_no_turns_still_walks_backwards() {
        let rows = turns_of(6);
        let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
        let (messages, rendered) = a_conversation(&borrowed);

        // Reached the way a client reaches one, from the cursor below it.
        let lower = page(&messages, &rendered, 0, None, 2);
        let cursor = lower.cursor.expect("there is a page above this one");

        let above = page(&messages, &rendered, 0, Some(&cursor), 0);

        assert!(!above.turns.is_empty(), "a page carries turns rather than none at all");
        assert_ne!(
            above.cursor.as_deref(),
            Some(cursor.as_str()),
            "and it moves rather than naming the turn it already opened at",
        );
    }

    /// The conversation's opening rows come before its first turn, so a page
    /// starting at that turn leaves them above every page, where no walk
    /// reaches them.
    #[test]
    fn the_rows_before_the_first_turn_ride_the_first_page() {
        let (messages, rendered) = a_conversation(&[
            r#"{"type":"assistant","uuid":"a0","message":{"id":"m0","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"a delivery arrived"}]}}"#,
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"hello"},"session_id":"s"}"#,
            r#"{"type":"result","uuid":"r1","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
        ]);

        let first = page(&messages, &rendered, 0, None, 10);

        assert_eq!(first.turns.len(), 1, "precondition: this conversation is one turn");
        assert_eq!(
            first.turns[0].messages.first().map(|frame| frame["uuid"].clone()),
            Some(Value::String("a0".to_owned())),
            "the row before the first turn rides that turn rather than sitting above every page: \
             {:?}",
            first.turns[0].messages,
        );
    }

    /// A conversation whose only user rows are envelopes still pages.
    ///
    /// A cron fire, a delivery and a peer card all draw as notices rather than
    /// as turns, so a session nobody typed into has no turn boundary at all.
    /// A page that listed no turns AND said `cursor: null` would tell a client
    /// there is nothing above a conversation it has never seen - and the walk
    /// would stop there, with the whole of it unreachable.
    #[test]
    fn a_conversation_with_no_turns_still_pages() {
        let (messages, rendered) = a_conversation(&[
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"[Cron]\n\nstand-up"},"session_id":"s"}"#,
            r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"morning"}]}}"#,
            r#"{"type":"result","uuid":"r1","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
        ]);

        assert!(
            rendered.turns.is_empty(),
            "precondition: nothing in this conversation opens a turn",
        );
        assert!(!messages.is_empty(), "precondition: and it has content");

        let page = page(&messages, &rendered, 0, None, 10);

        assert_eq!(page.turns.len(), 1, "the conversation rides one turn rather than none");
        assert_eq!(page.turns[0].messages.len(), messages.len(), "and that turn carries all of it");
        assert_eq!(page.cursor, None, "with nothing above it, which is the one honest null");
    }

    /// A user row carrying two blocks opens one turn, not two.
    ///
    /// The fold draws a unit per block, so it opens two spans at one message.
    /// A cursor names a message and resolves to the first match, so a second
    /// boundary at that index would hand a walker a page of its own before it
    /// moved - an empty one, since the two boundaries are the same message.
    #[test]
    fn a_user_row_with_two_blocks_opens_one_turn() {
        let (messages, rendered) = a_conversation(&[
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]},"session_id":"s"}"#,
            r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"reply"}]}}"#,
            r#"{"type":"result","uuid":"r1","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
        ]);

        assert_eq!(rendered.turns.len(), 2, "precondition: the fold opens a turn per block");
        assert_eq!(
            rendered.turns[0].opens_at, rendered.turns[1].opens_at,
            "precondition: and both open at the one message that carried them",
        );

        let page = page(&messages, &rendered, 0, None, 10);

        assert_eq!(page.turns.len(), 1, "one message is one turn on the page");
        assert_eq!(
            page.turns[0].messages.len(),
            messages.len(),
            "carrying the whole conversation, since both blocks open at its first row",
        );
        assert_eq!(page.cursor, None, "and the walk is not sent after a page that cannot move");
    }

    /// A page opens on a turn, and consecutive pages meet without a gap.
    #[test]
    fn a_page_opens_on_a_turn_and_the_pages_meet_exactly() {
        let rows = turns_of(50);
        let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
        let (messages, rendered) = a_conversation(&borrowed);
        let first = page(&messages, &rendered, 0, None, 10);

        // A page is the turns it was asked for, each opening where a turn
        // does. A page beginning anywhere else hands a client the tail of one
        // turn and no way to tell that is what it has.
        assert_eq!(first.turns.len(), 10, "a page is the turns it was asked for");
        // The page is the NEWEST ten, and it lists them in conversation order: a
        // client walking back gets each page in reading order and asks for the ones
        // above it.
        assert_eq!(
            opened_on(first.turns.first().expect("ten turns")),
            "turn 40",
            "the page reaches back exactly as far as it was asked for, no further",
        );
        assert_eq!(
            opened_on(first.turns.last().expect("ten turns")),
            "turn 49",
            "and it ends on the newest turn of the conversation",
        );

        // And the next page must reach back to where this one began. It may
        // repeat turns - the client keys them and drops the repeats - but it
        // may never skip one, because a skipped turn is history the reader has
        // no way to ask for again.
        let all = every_turn(&messages, &rendered);
        let second = page(&messages, &rendered, 0, first.cursor.as_deref(), 10);
        let second_turns: Vec<String> = second.turns.iter().map(opened_on).collect();

        assert!(!second_turns.is_empty(), "asking for more turns returns some");
        let above = all
            .iter()
            .position(|held| *held == opened_on(&first.turns[0]))
            .and_then(|at| at.checked_sub(1))
            .map(|at| all[at].clone());
        assert_eq!(
            second_turns.last().cloned(),
            above,
            "the page above ends on the turn directly above this one, so no turn is skipped",
        );
    }

    /// A client walking back through history reaches EVERY turn, and the walk
    /// TERMINATES.
    ///
    /// **This is the assertion the keyed cursor failed.** A cursor the fold
    /// could not supply is `None` on every page, and `None` is the one signal
    /// a client reads as "nothing above this page" - so the walk stopped after
    /// one page and every turn above it was unreachable. Not an error and not
    /// a gap a client can see: a conversation that appeared to begin where the
    /// page did. `pages > 1` is the half that catches it.
    #[test]
    fn walking_back_through_history_sees_every_turn_and_ends() {
        let rows = turns_of(25);
        let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
        let (messages, rendered) = a_conversation(&borrowed);
        let every = every_turn(&messages, &rendered);

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let asked = page(&messages, &rendered, 0, cursor.as_deref(), 5);
            pages += 1;
            assert!(pages < every.len() + 2, "the walk terminates rather than cycling");
            seen.extend(asked.turns.iter().map(opened_on));
            match asked.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert!(pages > 1, "the walk goes past the first page, or nothing above it is reachable");
        for turn in &every {
            assert!(seen.contains(turn), "every turn is reached, and {turn} was not");
        }
        assert!(
            seen.contains(&every[0]),
            "and the walk ends only once the OLDEST turn has been seen: {pages} pages",
        );
    }

    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::surface::PendingKind;
    use crate::testing::ViewFacts;
    use crate::transcript::ChatUnit;
    use crate::work::WorkCache;
    use forge_primitives::git_diff::GitDiffSnapshot;

    /// Where the fixture fleet is built. Fixed rather than per-run, so the
    /// fixture does not pin one machine's temp directory.
    const FIXTURE_ROOT: &str = "/tmp/forge-wire-fixture";

    /// The fixture's own repository at `path`: `main` with one commit, a
    /// `worktree-pr` branch one commit ahead of it, and uncommitted work -
    /// so the record's content read carries a worktree layer and a
    /// branch-ahead layer, both populated.
    ///
    /// Deterministic on purpose: the fixture pins the hunks this builds,
    /// word for word.
    fn fixture_repo(path: &Path) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                // The dates are pinned because the record now carries the
                // branch's commit chain, and a sha covers the commit's
                // timestamps: unpinned, every run minted a different sha and
                // the fixture could not hold one.
                .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+00:00")
                .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+00:00")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::create_dir_all(path).expect("the fixture's directory");
        git(&["init", "-q"]);
        git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&["config", "user.email", "fixture@example.test"]);
        git(&["config", "user.name", "Fixture"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(path.join("kept.txt"), "one\ntwo\n").expect("write kept.txt");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        git(&["checkout", "-q", "-b", "worktree-pr"]);
        std::fs::write(path.join("branch.txt"), "committed on the branch\n").expect("write");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "branch"]);
        // Uncommitted: a modification, and an addition staged so it is a
        // tracked change against HEAD.
        std::fs::write(path.join("kept.txt"), "one\ntwo\nthree\n").expect("write kept.txt");
        std::fs::write(path.join("added.txt"), "added one\n").expect("write added.txt");
        git(&["add", "added.txt"]);
    }

    /// The surface the fixtures are produced from, and both halves of the
    /// reason are deliberate.
    ///
    /// DETERMINISTIC, because the wire carries absolute paths and a fleet
    /// built in a per-run directory would pin the directory rather than the
    /// shape. POPULATED, because an empty fleet pins almost nothing: most of
    /// a session's fields would be `null`, and a field renamed to `null`
    /// would still pass.
    async fn fixture_state() -> TransportState {
        let root = Path::new(FIXTURE_ROOT);
        let _ = std::fs::remove_dir_all(root);
        let fleet =
            crate::testing::Fleet::in_dir(root, &[("TestOrg", &["proj"])]).expect("the fleet");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.set_cli_version(Some("1.0.0"), Some("1.1.0"));
        fleet.set_user_preferences(serde_json::json!({}));
        fleet.add_worker("TestOrg", "proj", "w1").expect("the worker is added");
        fleet.install_agent("TestOrg", "proj", "lead");
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &[
                    r#"{"type":"user","message":{"role":"user","content":"hello"},"session_id":"s"}"#,
                    // A tool call, so the fixture pins a group's rows: the
                    // class it keys on, the word it draws and the tool's own
                    // name.
                    r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls","description":"List files"}}]}}"#,
                    r#"{"type":"user","uuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"a\nb"}]}}"#,
                    // A counted turn, so the usage fixture carries real
                    // tokens rather than a pool that reads as empty. The
                    // stamp is fixed in the past: a "now" stamp would put
                    // the record in the rolling windows, and which of them
                    // hold it flips at midnight.
                    r#"{"type":"assistant","uuid":"a2","timestamp":"2025-01-02T03:04:05.000Z","message":{"id":"m-usage","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"counted"}],"usage":{"input_tokens":7,"output_tokens":11,"cache_read_input_tokens":13,"cache_creation_input_tokens":17}}}"#,
                    // A sub-agent dispatch, so the record carries a
                    // POPULATED card rather than an empty list a rename
                    // could cross unseen.
                    r#"{"type":"assistant","uuid":"a3","message":{"id":"m-sub","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"tu-sub","name":"Task","input":{"description":"map the calls","subagent_type":"general-purpose","run_in_background":false,"prompt":"do the thing"}}]}}"#,
                    r#"{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
                ],
            )
            .expect("the transcript seeds");
        fleet.seed_test_pending_interaction(&fixture_seat(), PendingKind::Permission);
        // A rostered background task, so the fixture carries a populated
        // registry rather than an empty list a reader cannot tell from an
        // unwired field - and the two sub-agent facts, which the session
        // task's one fold raises and this fixture has no task to run: the
        // dispatch the transcript seeded, as the card list and the flag a
        // seat that ran it would hold.
        fleet.seed_view_facts(
            &fixture_seat(),
            ViewFacts {
                background_tasks: vec![forge_workspace::BackgroundTask {
                    task_id: "t-fixture".to_owned(),
                    task_type: "local_bash".to_owned(),
                    description: "gh run watch".to_owned(),
                    command: Some("gh run watch 123 --exit-status".to_owned()),
                    tool_use_id: Some("tu-fixture".to_owned()),
                }],
                cards: vec![forge_primitives::runtime::SubagentCard {
                    name: "map the calls".to_owned(),
                    dispatch_id: "tu-sub".to_owned(),
                    agent_type: Some("general-purpose".to_owned()),
                    running: false,
                    failed: false,
                    backgrounded: false,
                    ended_at_ms: Some(1_750_000_000_000),
                    calls: 1,
                    tail: vec![forge_primitives::runtime::SubagentCall {
                        name: "Read".to_owned(),
                        title: "Read src/lib.rs".to_owned(),
                        status: forge_primitives::runtime::SubagentCallStatus::Completed,
                    }],
                    usage: Some(forge_primitives::messages::TaskUsage {
                        total_tokens: 9_714,
                        tool_uses: 1,
                        duration_ms: 2_716,
                        extras: serde_json::Map::new(),
                    }),
                }],
                ..ViewFacts::default()
            },
        );
        // A scan the fixture pins the working tree, the PR row and the
        // diff content from. It is taken of a repository of its own beside
        // the fleet rather than of the seat's project directory: building
        // one AT that path would also make the path exist, and the home
        // fixture's rows - pinned with that directory gone - would move
        // with it. The scan is real, so the record's content read has both
        // layers to carry rather than pinning a failed layer; the PR row
        // is stamped on top, since a temp repo has no remote for `gh` to
        // find one through.
        let surface = fleet.surface();
        let work = Arc::new(WorkCache::new());
        let repo = Path::new(FIXTURE_ROOT).join("diff-repo");
        fixture_repo(&repo);
        let mut diff = forge_workspace::env::git_diff::scan(&repo, None).await;
        diff.pr = Some(forge_primitives::git::GitPrInfo {
            number: 1249,
            url: "https://example.test/pull/1249".to_owned(),
            draft: false,
        });
        diff.closes = vec![forge_primitives::git::GitIssueRef {
            number: 1215,
            url: "https://example.test/issues/1215".to_owned(),
        }];
        diff.pushed_sha = Some("abc123".to_owned());
        diff.pr_fetched_at = None;
        surface.store_work_snapshot(
            &fixture_seat(),
            forge_workspace::work::WorkSnapshot {
                diff,
                cwd: repo,
                read_at: std::time::Instant::now(),
            },
        );

        let state = TransportState {
            surface,
            work,
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };
        // The transport does not read a transcript, so the seat's
        // conversation is put where a `Connected` would have left it.
        fleet
            .hold_conversation(&state, "TestOrg", "proj", "lead")
            .expect("the fixture's conversation is held");
        state
    }

    /// The substrings a fixture must not pin: the directory the fleet
    /// happens to live in, in every form the wire spells it.
    ///
    /// Three forms, because the wire carries all three. A path arrives raw
    /// (`ProjectView::path`), canonically (anything that resolved a symlink)
    /// and SLUGGED (`ProjectKey`, every non-alphanumeric turned into `-`).
    /// The slug form is why replacing the plain path alone is not enough:
    /// the key is a temp directory spelled with dashes, and it differs per
    /// machine just as much as the path does.
    fn volatile(root: &Path) -> Vec<(String, &'static str)> {
        let raw = root.to_string_lossy().into_owned();
        let canonical = root
            .canonicalize()
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let mut forms = vec![
            (raw.clone(), "<fixture>"),
            (canonical.clone(), "<fixture>"),
            (slug(&raw), "<fixture>"),
            (slug(&canonical), "<fixture>"),
            // The build stamp carries the commit the binary was built from, so
            // it moves on every commit and a fixture pinning it would fail on
            // the next one. Each gets its OWN stand-in: collapsing them to one
            // placeholder would leave a client unable to tell which field is
            // which.
            (crate::FORGE_VERSION.to_owned(), "<forge-version>"),
            (crate::FORGE_VERSION_SHORT.to_owned(), "<forge-version-short>"),
        ];
        forms.sort_by_key(|(form, _)| std::cmp::Reverse(form.len()));
        forms.dedup_by(|left, right| left.0 == right.0);
        forms
    }

    /// The slug `ProjectKey` builds: every non-alphanumeric becomes `-`.
    fn slug(path: &str) -> String {
        path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
    }

    fn normalise(value: &mut Value, forms: &[(String, &'static str)]) {
        match value {
            Value::String(text) => {
                for (form, stands_in) in forms {
                    if let Some(rest) = text.strip_prefix(form.as_str()) {
                        *text = format!("{stands_in}{rest}");
                        break;
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| normalise(item, forms)),
            Value::Object(fields) => fields.values_mut().for_each(|field| normalise(field, forms)),
            _ => {}
        }
    }

    /// Every subject a client can watch, with the fixture that pins its wire
    /// shape.
    fn fixtures() -> Vec<(Subject, PathBuf)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire_fixtures");
        vec![
            (Subject::Home, dir.join("home.json")),
            (Subject::Session(fixture_seat()), dir.join("session.json")),
            (Subject::Usage, dir.join("usage.json")),
        ]
    }

    /// Writes each subject's fixture from the encoder. Run deliberately:
    /// `cargo nextest run -p forge-server --run-ignored ignored-only write_the_fixtures`.
    ///
    /// **What this writes is only what the encoder currently emits**, so a
    /// fixture produced here pins the present shape and a wrong one equally.
    /// Each has to be READ against the intended shape before it is
    /// committed; generating one is not the work.
    #[tokio::test]
    #[ignore = "writes the fixtures; run deliberately"]
    async fn write_the_fixtures() {
        let state = fixture_state().await;
        let forms = volatile(Path::new(FIXTURE_ROOT));
        std::fs::create_dir_all(fixtures()[0].1.parent().expect("a directory")).expect("mkdir");
        for (subject, fixture) in fixtures() {
            let mut encoded = encode_subject(&state, &subject).await.expect("encode");
            normalise(&mut encoded, &forms);
            std::fs::write(&fixture, serde_json::to_string_pretty(&encoded).expect("render"))
                .expect("write");
        }
    }

    /// A seat's own working tree crosses on its row, which is what lets a
    /// worker's row draw its own branch rather than its project's.
    ///
    /// Read through the same shared cache the project rows use, so this is one
    /// read drawn twice rather than two reads - and it is a read a page cannot
    /// derive from the project's, because the two name different directories.
    #[tokio::test]
    async fn a_home_agents_row_carries_its_own_tree() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Home).await.expect("encode");
        let row = encoded["agents"]
            .as_array()
            .expect("agents is a list")
            .iter()
            .find(|row| row["slot"]["label"] == "lead")
            .expect("the started project's lead is a row");

        assert!(
            row.get("work").is_some(),
            "the seat's own tree crosses on the row, not the project's read: {row}"
        );
    }

    /// The rows the processes feed leads with: the CLI's background-task
    /// registry, which no read answered.
    ///
    /// Without it a client drew the OS walk and nothing else, so a
    /// `setsid`-detached bash - the case the registry exists for - was a row
    /// the terminal had and the client could not: not an empty field, a
    /// missing row.
    #[tokio::test]
    async fn a_session_snapshot_carries_the_background_registry() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.seed_view_facts(
            &fixture_seat(),
            ViewFacts {
                background_tasks: vec![forge_workspace::BackgroundTask {
                    task_id: "t1".to_owned(),
                    task_type: "local_bash".to_owned(),
                    description: "gh run watch".to_owned(),
                    command: Some("gh run watch 123 --exit-status".to_owned()),
                    tool_use_id: Some("tu-1".to_owned()),
                }],
                ..ViewFacts::default()
            },
        );
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");

        assert_eq!(
            encoded["background_tasks"][0]["task_id"], "t1",
            "the seat's registry crosses: {encoded}"
        );
        assert_eq!(
            encoded["background_tasks"][0]["description"], "gh run watch",
            "with the line the row leads with: {encoded}"
        );
        assert_eq!(
            encoded["background_tasks"][0]["command"], "gh run watch 123 --exit-status",
            "and the command the scan adopts by: {encoded}"
        );
    }

    /// The occupant's id crosses on the header: the `Connected` that named it
    /// reaches nobody who was not attached when it fired, so a page opened on
    /// a running seat has this rather than the frame. The worker registry
    /// carries the same id for a live dynamic worker, and nothing else does.
    #[tokio::test]
    async fn a_session_snapshot_carries_the_occupants_id() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.seed_view_facts(
            &fixture_seat(),
            ViewFacts {
                session_id: Some(forge_primitives::SessionId::new("d4f70669-1f2a")),
                ..ViewFacts::default()
            },
        );
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");

        assert_eq!(
            encoded["header"]["session_id"], "d4f70669-1f2a",
            "the occupant the core named crosses on the header: {encoded}"
        );
    }

    /// **The dispatch flag the record carries is the workspace's.** It was
    /// computed here, over the held conversation, until the push landed: the
    /// fold that raises it is the one that announces the raise, so the record
    /// reads it through the view surface rather than keeping a second answer
    /// that could disagree with the frame.
    #[tokio::test]
    async fn the_record_reads_the_dispatch_flag_from_the_workspace() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let before =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");
        assert_eq!(
            before["has_dispatches"], false,
            "a seat whose fold raised nothing reads false: {before}",
        );

        fleet.seed_view_facts(
            &fixture_seat(),
            ViewFacts { has_dispatches: true, ..ViewFacts::default() },
        );

        let after =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");
        assert_eq!(
            after["has_dispatches"], true,
            "and the flag the workspace holds is the one the record answers with: {after}",
        );
    }

    /// **The instance list the record carries is the workspace's**, not a
    /// fold the record takes over the conversation.
    ///
    /// It was folded here until the cap landed - over the whole held
    /// conversation, on every read - and the two answers part company the
    /// moment the conversation a record can see is shorter than the one the
    /// session has run: a section that shrank with the cap would be one no
    /// viewer of the session's own frames agrees with. The seat below
    /// dispatches in its conversation, so a fold here would answer a card,
    /// and what the record carries has to be the workspace's rather than it.
    #[tokio::test]
    async fn the_record_reads_the_instance_list_from_the_workspace() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &[
                    r#"{"type":"user","message":{"role":"user","content":"map it"},"session_id":"s"}"#,
                    r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"tu-sub","name":"Task","input":{"description":"from the conversation","subagent_type":"Explore"}}]}}"#,
                ],
            )
            .expect("the transcript seeds");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };
        fleet
            .hold_conversation(&state, "TestOrg", "proj", "lead")
            .expect("the fixture's conversation is held");

        let before =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");
        assert_eq!(
            before["subagent_instances"].as_array().expect("a list").len(),
            0,
            "the conversation's own dispatch is not folded by the read: {before}",
        );

        fleet.seed_view_facts(
            &fixture_seat(),
            ViewFacts {
                has_dispatches: true,
                cards: vec![forge_primitives::runtime::SubagentCard {
                    name: "from the workspace".to_owned(),
                    dispatch_id: "tu-sub".to_owned(),
                    agent_type: Some("Explore".to_owned()),
                    running: true,
                    failed: false,
                    backgrounded: false,
                    ended_at_ms: None,
                    calls: 0,
                    tail: Vec::new(),
                    usage: None,
                }],
                ..ViewFacts::default()
            },
        );

        let after =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");
        let cards = after["subagent_instances"].as_array().expect("a list");
        assert_eq!(
            cards.len(),
            1,
            "and the list the workspace holds is the one that crosses: {after}"
        );
        assert_eq!(
            cards[0]["name"], "from the workspace",
            "named by the fold that announces it rather than by a second one here: {after}",
        );
    }

    /// **A seat whose loop never ran is walked at the read**, the same
    /// fallback the work row takes. A fixture holds no seat, so no loop runs
    /// for one: the record's index is the tree's rather than an empty list
    /// standing in for a tree nobody looked at.
    #[tokio::test]
    async fn the_record_walks_the_index_when_the_store_holds_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("found.rs"), "").expect("write");
        let fleet = crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");

        let entries = encoded["file_index"]["entries"].as_object().expect("an index crosses");
        assert!(
            entries.contains_key("found.rs"),
            "a seat nothing has walked is walked at the read: {encoded}",
        );
    }

    /// The PR row the inspector leads its GIT section with: the open pull
    /// request, and the issues it closes.
    ///
    /// The keys are asserted PRESENT rather than populated, because a
    /// populated one needs `gh`, a pushed branch and an open PR - which this
    /// fixture cannot produce. The key's presence is the property that matters
    /// here: a client drawing the section reads a field that is there and
    /// null, rather than finding no field at all.
    ///
    /// The POPULATED shape is pinned next door, from a seeded scan, and in
    /// the committed fixture; the live walk that reached a real PR is in the
    /// plan's ledger at
    /// `docs/superpowers/ledgers/2026-09-28-forge-server-socket/` (git-excluded,
    /// so no repo-visible artifact carries it).
    #[tokio::test]
    async fn a_session_snapshot_carries_the_pr_row() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");

        assert!(encoded.get("pr").is_some(), "the session carries a PR field: {encoded}");
        assert_eq!(
            encoded["closes"],
            serde_json::json!([]),
            "and the issues it closes, empty when there is no PR: {encoded}"
        );
    }

    /// The record carries the changed files' hunks, bounded and flagged: a
    /// review surface has no other read of them.
    ///
    /// Its own fleet and its own repository rather than `fixture_state`'s
    /// shared root: that root is one fixed path, and two tests calling it
    /// in one binary's parallel run unlink it under each other.
    #[tokio::test]
    async fn the_record_carries_the_changed_files_with_their_hunks() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let seat = fixture_seat();
        let surface = fleet.surface();
        let repo = tempfile::tempdir().expect("a repository directory");
        fixture_repo(repo.path());
        let diff = forge_workspace::env::git_diff::scan(repo.path(), None).await;
        surface.store_work_snapshot(
            &seat,
            forge_workspace::work::WorkSnapshot {
                diff,
                cwd: repo.path().to_owned(),
                read_at: std::time::Instant::now(),
            },
        );
        let state = TransportState {
            surface,
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Session(seat)).await.expect("encode");

        let files = encoded["diff"]["worktree"]["populated"]
            .as_array()
            .expect("the worktree layer is populated");
        let added = files.iter().find(|file| file["path"] == "added.txt").expect("added.txt");
        assert_eq!(added["status"], "added", "the status crosses: {encoded}");
        let lines = added["hunks"][0]["lines"].as_array().expect("hunks carry their lines");
        assert!(
            lines.iter().any(|line| line["kind"] == "added" && line["text"] == "added one"),
            "the hunk's own lines cross: {encoded}",
        );
        assert_eq!(encoded["diff"]["truncated"], false, "nothing was withheld");
        let ahead = encoded["diff"]["branch_ahead"]["populated"]
            .as_array()
            .expect("the branch layer is populated too");
        assert!(
            ahead.iter().any(|file| file["path"] == "branch.txt"),
            "and the branch's own commit is there: {encoded}",
        );
    }

    /// A POPULATED PR row, from a seeded scan rather than from `gh`.
    ///
    /// The key's presence is not enough on its own: a regression that answers
    /// no PR - a wrong directory handed to the read, say - keeps the key and
    /// passes that. This drives the values a client draws.
    #[tokio::test]
    async fn the_pr_row_carries_the_scans_pr_and_its_closing_issues() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let seat = fixture_seat();
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");
        surface.store_work_snapshot(
            &seat,
            forge_workspace::work::WorkSnapshot {
                diff: scanned(),
                cwd: std::path::PathBuf::from(&cwd),
                read_at: std::time::Instant::now(),
            },
        );
        let work = Arc::new(WorkCache::new());
        let state = TransportState {
            surface,
            work,
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Session(seat)).await.expect("encode");

        assert_eq!(encoded["pr"]["number"], 1249, "the open PR crosses with its number: {encoded}");
        assert_eq!(encoded["closes"][0]["number"], 1215, "and the issues it closes: {encoded}");
        assert_eq!(
            encoded["work"]["branch"], "worktree-pr",
            "and the branch comes from the SAME scan as the PR, so the two cannot disagree: \
             {encoded}"
        );
        assert_eq!(encoded["work"]["changed"], 3, "with the count that scan took: {encoded}");
    }

    /// The pushed row and the record's own fields are one answer.
    ///
    /// Both are the seat's store read the same way, so a client that applies
    /// the update lands on exactly what a client that read the record holds -
    /// a second derivation is the only way the two could ever disagree.
    #[tokio::test]
    async fn the_pushed_row_is_the_records_own_fields() {
        // Its OWN fleet rather than `fixture_state`'s shared root: that root is
        // one fixed path, and two tests calling it in one binary's parallel
        // run unlink it under each other.
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let seat = fixture_seat();
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");
        surface.store_work_snapshot(
            &seat,
            forge_workspace::work::WorkSnapshot {
                diff: scanned(),
                cwd: std::path::PathBuf::from(&cwd),
                read_at: std::time::Instant::now(),
            },
        );
        let state = TransportState {
            surface,
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded =
            encode_subject(&state, &Subject::Session(seat.clone())).await.expect("encode");
        let held = state.surface.work(&seat, std::path::Path::new(&cwd)).await;
        let pushed = forge_workspace::work::work_from_scan(&held.diff, &held.cwd);

        assert_eq!(
            encoded["work"],
            serde_json::to_value(&pushed).expect("encode"),
            "the row the seat pushes is the row its record answers",
        );
        assert_eq!(
            encoded["pr"],
            serde_json::to_value(&held.diff.pr).expect("encode"),
            "and so is the PR beside it",
        );
        assert_eq!(
            encoded["closes"],
            serde_json::to_value(&held.diff.closes).expect("encode"),
            "and the issues it closes",
        );
        assert_eq!(
            encoded["git"],
            serde_json::to_value(forge_workspace::work::git_work_view(&held.diff)).expect("encode"),
            "and the tree behind the row's depth",
        );
    }

    /// A scan as the terminal's inspector reads it: one branch, one worktree
    /// layer, one chain ahead, one PR and one closing issue. Both depth
    /// layers are populated so the record-vs-constructor guard covers both
    /// axes - a derivation omitting either reddens.
    fn scanned() -> GitDiffSnapshot {
        use forge_primitives::git::{GitBranch, GitCommit, GitIssueRef, GitPrInfo};
        use forge_primitives::git_diff::{GitBranchAhead, GitDiffStats, LayerState, RepoGate};
        GitDiffSnapshot {
            branch: GitBranch::Named("worktree-pr".to_owned()),
            pushed_sha: Some("abc123".to_owned()),
            pr_fetched_at: None,
            default_branch: Some("main".to_owned()),
            repo_gate: RepoGate::InRepo,
            worktree: LayerState::Populated(GitDiffStats {
                files: Vec::new(),
                total_files: 3,
                total_added: 9,
                total_removed: 2,
            }),
            branch_ahead: LayerState::Populated(GitBranchAhead {
                commit_count: 1,
                stats: GitDiffStats {
                    files: Vec::new(),
                    total_files: 0,
                    total_added: 0,
                    total_removed: 0,
                },
                commits: vec![GitCommit {
                    sha: "a1b2c3d".to_owned(),
                    subject: "the branch's own commit".to_owned(),
                    stats: GitDiffStats::default(),
                    time: 1_766_000_000,
                }],
            }),
            pr: Some(GitPrInfo {
                number: 1249,
                url: "https://example.test/pull/1249".to_owned(),
                draft: false,
            }),
            closes: vec![GitIssueRef {
                number: 1215,
                url: "https://example.test/issues/1215".to_owned(),
            }],
        }
    }

    /// The token/cost report, on the subject a usage view subscribes to.
    /// Nothing carried it: the pool is scanned off-thread by the terminal,
    /// and a client had no way to ask for the same numbers.
    ///
    /// Its own fleet rather than the shared fixture root, because the scan
    /// reads that root's transcript pool: two tests building the same root at
    /// once is a fight over one directory.
    #[tokio::test]
    async fn a_usage_subscription_is_answered_with_the_pools_report() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        // A stamp in the past on purpose: a record stamped "now" flips which
        // rolling windows hold it at midnight, so the assertion below would be
        // a coin toss once a day.
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &[r#"{"type":"assistant","timestamp":"2025-01-02T03:04:05.000Z","message":{"id":"m-usage","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"counted"}],"usage":{"input_tokens":7,"output_tokens":11}}}"#],
            )
            .expect("the transcript seeds");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Usage).await.expect("encode");

        assert_eq!(
            encoded["lifetime"]["total"]["input"], 7,
            "the pool's own count crosses, so the read is the scanner's: {encoded}"
        );
        assert_eq!(
            encoded["today"]["total"]["input"], 0,
            "and a record from a past day is in no rolling window: {encoded}"
        );
        assert!(
            encoded.get("pricing_available").is_some(),
            "and the flag that blanks costs rather than reading a misleading zero: {encoded}"
        );
    }

    /// What one fixture pins, and what it only appears to.
    ///
    /// The byte-compare above makes a renamed field fail, and it is silent
    /// about how much it is covering. A key the fixture carries as `null`
    /// pins the key's existence and nothing behind it; an ARRAY it carries
    /// empty pins nothing about the element shape at all, so a field renamed
    /// inside one is invisible to every pin in the tree. `projects[]/tasks[]`
    /// and `projects[]/crons[]` are on that list: named on every run and not
    /// yet seeded, which is #1367.
    ///
    /// Counted rather than listed, and printed on every run: the number is
    /// what says whether the fixture is the instrument it is taken for.
    #[derive(Default)]
    struct Coverage {
        keys: usize,
        nulls: BTreeSet<String>,
        empty_arrays: BTreeSet<String>,
    }

    fn coverage(value: &Value, path: &str, out: &mut Coverage) {
        match value {
            Value::Object(fields) => {
                for (key, held) in fields {
                    out.keys += 1;
                    let here = format!("{path}/{key}");
                    if held.is_null() {
                        out.nulls.insert(here.clone());
                    }
                    coverage(held, &here, out);
                }
            }
            Value::Array(items) => {
                if items.is_empty() {
                    out.empty_arrays.insert(format!("{path}[]"));
                }
                for item in items {
                    coverage(item, &format!("{path}[]"), out);
                }
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn every_subject_round_trips_through_its_fixture() {
        let state = fixture_state().await;
        let forms = volatile(Path::new(FIXTURE_ROOT));
        let mut keys = 0usize;
        let mut nulls = 0usize;
        let mut empty: BTreeSet<String> = BTreeSet::new();

        for (subject, fixture) in fixtures() {
            let mut encoded = encode_subject(&state, &subject).await.expect("encode");
            normalise(&mut encoded, &forms);
            let expected: Value =
                serde_json::from_str(&std::fs::read_to_string(&fixture).expect("read"))
                    .expect("parse");
            assert_eq!(encoded, expected, "{} changed shape", fixture.display());

            let mut pin = Coverage::default();
            coverage(&encoded, "", &mut pin);
            keys += pin.keys;
            nulls += pin.nulls.len();
            empty.extend(pin.empty_arrays);
        }

        // The floor: an empty fixture compares equal to a snapshot of nothing
        // and would otherwise report as clean as a populated one.
        assert!(keys > 0, "no fixture carries a key, so there is nothing to pin");
        eprintln!(
            "wire fixtures: {keys} key slots carried, {nulls} null (the key is named, nothing \
             behind it is pinned), {} empty collections (no element shape pinned anywhere): {}",
            empty.len(),
            empty.iter().cloned().collect::<Vec<_>>().join(", "),
        );
    }

    /// A client could set a session's dictation and move the process's input,
    /// and no record carried either back: three commands crossed the wire and
    /// nothing read what they had set.
    #[tokio::test]
    async fn the_dictation_a_client_set_comes_back() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let seat = fixture_seat();
        // A session has to exist for the override to land on, which is why
        // this is a seat with a live agent rather than a bare project.
        fleet.install_agent("TestOrg", "proj", "lead");
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");

        surface
            .dispatch(crate::Command::SetDictateOverride {
                key: seat.clone(),
                update: forge_workspace::DictateOverrideUpdate::Styling(
                    forge_dictate::normalize::Styling::SemiFormal,
                ),
            })
            .expect("the session's override is applied");
        surface
            .dispatch(crate::Command::SetDictateDevice {
                key: seat.clone(),
                pick: Some(forge_workspace::DictateDeviceChoice::Device("a-mic".to_owned())),
            })
            .expect("the device pick is applied");

        let state = TransportState {
            surface: Arc::clone(&surface),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let session = encode_subject(&state, &Subject::Session(seat)).await.expect("encode");
        assert_eq!(
            session["state"]["dictate_overrides"]["styling"], "semi_formal",
            "the override the client set is the override it reads back: {session}",
        );

        let home = encode_subject(&state, &Subject::Home).await.expect("encode");
        assert_eq!(
            home["dictate"]["device"]["device"], "a-mic",
            "and the device it moved to is on the dictate read: {home}",
        );
        let _ = cwd;
    }

    /// The push-to-talk key and its press mapping reach the composer's record.
    ///
    /// A client draws the affordance the keyboard drives, and this is its only
    /// read of what `forge.toml` configured: without it the key is whatever
    /// the client hardcodes, which is the same for every install.
    #[tokio::test]
    async fn the_composer_carries_the_configured_bind_and_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).expect("the project's directory");
        std::fs::write(
            forge.join("forge.toml"),
            format!(
                "\
[[orgs]]
name = \"TestOrg\"
accounts = [\"Acct\"]

[[orgs.projects]]
name = \"proj\"
path = \"{}\"

[[accounts]]
display_name = \"Acct\"
token = \"t\"
models = [\"claude-sonnet-5\"]
provider = \"anthropic\"

[dictate]
enabled = true
bind = \"left_cmd\"
mode = \"toggle\"
",
                project.display()
            ),
        )
        .expect("write forge.toml");
        let workspace = Arc::new(
            forge_workspace::Workspace::new_for_test(dir.path().to_owned()).expect("workspace"),
        );
        let state = TransportState {
            surface: Arc::new(ViewSurface::new(workspace)),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let session =
            encode_subject(&state, &Subject::Session(fixture_seat())).await.expect("encode");
        assert_eq!(
            session["composer"]["bind"], "left_cmd",
            "the composer carries the configured push-to-talk key, not a default: {session}",
        );
        assert_eq!(
            session["composer"]["mode"], "toggle",
            "and how a press maps onto a take, not a default either: {session}",
        );
    }

    /// The account a project's row chips, and its state.
    ///
    /// The state is derived from the project's model and the account pool, so
    /// `has_model` is not enough to draw it from - a client could draw the row
    /// and not the chip on it.
    ///
    /// The fixture's projects declare no model, so nothing binds and the chip
    /// is absent here: what this pins is the field being on the row at all,
    /// and the POPULATED case is the acceptance walk's, against a config whose
    /// projects do declare one.
    #[tokio::test]
    async fn a_projects_row_carries_the_chip_field() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Home).await.expect("encode");
        let rows = encoded["projects"].as_array().expect("the home carries project rows");

        assert!(rows[0].get("chip").is_some(), "the row carries a chip: {}", rows[0]);
        assert_eq!(
            rows[0]["project"]["has_model"], false,
            "and this fixture's project declares no model, which is why the chip is absent",
        );
    }

    /// The terminal draws a project's SCHEDULES section from a read the wire
    /// never carried, so a client could create a cron and never see it again.
    #[tokio::test]
    async fn a_projects_schedules_ride_its_row() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.add_cron("proj", "a nightly sweep").expect("the cron is added");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Home).await.expect("encode");
        let rows = encoded["projects"].as_array().expect("the home carries project rows");
        let crons = rows[0]["crons"].as_array().expect("and each row carries its schedules");

        assert_eq!(crons.len(), 1, "the project's own cron reached the wire: {crons:?}");
        assert_eq!(crons[0]["prompt"], "a nightly sweep");
    }

    /// A project's connector subscriptions ride its row, which is the read the
    /// section draws.
    ///
    /// The home's own `connectors` carried a `subscriptions` list that no read
    /// could fill - it was built with no project scope, and `None` means "the
    /// liveness facts only" - so a page drawing that list showed the connector
    /// section empty however long it waited, and would have flip-flopped
    /// against the next re-read once a frame patched it.
    #[tokio::test]
    async fn a_projects_connector_subscriptions_ride_its_row() {
        let (workspace, _dir) = crate::surface::testing::workspace_with_connector_subs();
        let state = TransportState {
            surface: Arc::new(crate::surface::ViewSurface::new(Arc::clone(&workspace))),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Home).await.expect("encode");
        let rows = encoded["projects"].as_array().expect("the home carries project rows");
        let row = rows
            .iter()
            .find(|row| row["project"]["name"] == "forge")
            .unwrap_or_else(|| panic!("the project has a row: {encoded}"));

        assert_eq!(
            row["connectors"]["gotify"].as_array().map(Vec::len),
            Some(1),
            "the project's own gotify subscription is on its row: {encoded}",
        );
        assert_eq!(
            row["connectors"]["gotify"][0]["applications"][0], "forge",
            "as the subscription itself, not a placeholder",
        );
        assert_eq!(
            row["connectors"]["slack"].as_array().map(Vec::len),
            Some(1),
            "and so is its slack subscription: {encoded}",
        );

        // The home-level `connectors` is the liveness facts and nothing else:
        // a subscription field a stale reader still looks for is a section
        // that draws empty forever.
        assert!(
            encoded["connectors"]["gotify"].get("subscriptions").is_none(),
            "the home's own connectors carries no subscription list: {encoded}",
        );
        assert!(
            encoded["connectors"]["slack"].get("subscriptions").is_none(),
            "for either connector: {encoded}",
        );
        assert!(
            encoded["connectors"]["slack"]["connected_workspaces"].is_array(),
            "the facts it does carry are still there: {encoded}",
        );
    }

    /// A worker's row draws ITS OWN working tree, not its project's.
    ///
    /// The home carried one `work` per project, read at the project's own path
    /// from the lead's seat, so every worker row under it drew that branch and
    /// that count. On a fleet with a worker in a worktree that is a plausible
    /// wrong answer rather than an obviously missing one - `main` on a worker's
    /// row reads as the worker's branch - which is why the client drew no
    /// branch at all on a worker's row rather than another seat's.
    #[tokio::test]
    async fn a_workers_row_carries_its_own_tree_and_not_the_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The trees go up BEFORE the fleet does. A project's registry key is
        // derived from its path and the derivation canonicalises, so a key
        // taken while the directory is absent is not the key taken once it is
        // there - and a worker registered under the first is registered under
        // one nothing looks up again.
        let root = dir.path().join("proj");
        let worktree = root.join(".claude/worktrees/w1");
        repo_at(&root, "main", 1);
        repo_at(&worktree, "worktree-em-dash-sweep", 2);

        let fleet = crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.add_git_worker("TestOrg", "proj", "w1").expect("the worker is added");

        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Home).await.expect("encode");
        let rows = encoded["agents"].as_array().expect("the home carries agent rows");
        let row_of = |label: &str| {
            rows.iter().find(|row| row["label"] == label).expect("the seat has a row")
        };

        // The premise, so a row with no tree at all cannot pass as a row with
        // the right one: the worker's seat resolves to its own worktree.
        assert_eq!(
            state
                .surface
                .roster()
                .cwd_for(&SessionSlot::worker("TestOrg", "proj", "w1"))
                .as_deref(),
            Some(worktree.as_path()),
            "precondition: the worker's own directory is the worktree",
        );
        assert_eq!(
            row_of("lead")["work"]["branch"],
            "main",
            "the lead's row draws the project's own tree: {encoded}",
        );
        assert_eq!(row_of("lead")["work"]["changed"], 1, "with the count that tree has: {encoded}");
        assert_eq!(
            row_of("w1")["work"]["branch"],
            "worktree-em-dash-sweep",
            "and the worker's row draws its own, which the project's read cannot answer: \
             {encoded}",
        );
        assert_eq!(
            row_of("w1")["work"]["changed"],
            2,
            "with the worker's own count beside it: {encoded}",
        );
    }

    /// A seat forge holds no directory for draws NO tree, rather than the
    /// project's.
    ///
    /// The row is real: a despawned worker leaves its label behind and is
    /// drawn until the label goes, and the registry has nothing to compose a
    /// directory from. `None` is what says so, and the tempting repair - fall
    /// back to `ProjectWire::work` so the cell is not blank - is the bug this
    /// whole read exists to remove, one row narrower.
    #[tokio::test]
    async fn a_seat_with_no_directory_borrows_no_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("proj");
        repo_at(&root, "main", 1);

        let fleet = crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.add_despawned_worker("proj", "gone").expect("the label is left behind");

        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };

        let encoded = encode_subject(&state, &Subject::Home).await.expect("encode");
        let rows = encoded["agents"].as_array().expect("the home carries agent rows");
        let row_of = |label: &str| {
            rows.iter().find(|row| row["label"] == label).expect("the seat has a row")
        };

        assert_eq!(
            state.surface.roster().cwd_for(&SessionSlot::worker("TestOrg", "proj", "gone")),
            None,
            "precondition: forge holds no directory for the despawned label",
        );
        assert_eq!(
            row_of("gone")["work"],
            Value::Null,
            "so its row draws no tree rather than borrowing the project's: {encoded}",
        );
        assert_eq!(
            row_of("lead")["work"]["branch"],
            "main",
            "while the project's own row still draws the project's tree: {encoded}",
        );
    }

    /// A git repository at `dir` on `branch`, with `changed` uncommitted files.
    ///
    /// `.claude/` is ignored so a worker's worktree, nested inside the
    /// project, does not land in the project's own count as an untracked
    /// directory.
    fn repo_at(dir: &Path, branch: &str, changed: usize) {
        std::fs::create_dir_all(dir).expect("mkdir");
        git(dir, &["init", "-q"]);
        git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]);
        git(dir, &["config", "user.email", "t@e.com"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join(".gitignore"), ".claude/\n").expect("write");
        std::fs::write(dir.join("tracked.txt"), "one\n").expect("write");
        git(dir, &["add", ".gitignore", "tracked.txt"]);
        git(dir, &["commit", "-qm", "init"]);
        for file in 0..changed {
            std::fs::write(dir.join(format!("changed-{file}.txt")), "x\n").expect("write");
        }
    }

    /// Spawn git with the ambient repo-location variables scrubbed, as the
    /// product's own constructor does: a fixture that skipped it would answer
    /// about a foreign repository when the suite runs under a git hook.
    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}
