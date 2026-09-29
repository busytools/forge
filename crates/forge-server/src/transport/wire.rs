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

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use forge_primitives::review::{ReviewSet, ReviewThread};
use forge_primitives::runtime::{AvailableAgent, AvailableCommand, MonitorRecord};
use forge_primitives::slack::SlackSubscription;
use forge_primitives::{GotifySubscription, SessionSlot};
use forge_workspace::env::processes::{ProcessSnapshot, SCAN_STALENESS, scan};
use forge_workspace::{AccountLoadingRow, GatewayOrgView, McpServers, ProjectView, WorkerEntry};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::composer::{NoticeWire, Phase, SignIn};
use crate::file_index::FileIndex;
use crate::surface::{AgentRow, ConversationHistory, PendingAsk, ViewSurface};
use crate::transcript::ChatUnit;
use crate::transport::TransportState;
use crate::transport::envelope::Subject;
use crate::work::{WorkState, work_from_scan};

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
    pub peer: forge_primitives::PeerInflightStats,
    pub peer_failure_at: Option<std::time::SystemTime>,
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
            peer: row.peer.clone(),
            peer_failure_at: row.peer_failure_at,
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

/// What the inbound connectors are watching.
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
    pub subscriptions: Vec<GotifySubscription>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SlackWire {
    /// Workspace label to whether that workspace's pump is live, as a list
    /// of pairs: JSON objects with dynamic keys are awkward for a client to
    /// iterate, and the order is the store's.
    pub connected_workspaces: Vec<(String, bool)>,
    pub load_failed: bool,
    pub subscriptions: Vec<SlackSubscription>,
}

/// Dictation's preflight state.
///
/// **The device catalog is not here and nothing asks for it.** Enumerating
/// devices is a blocking walk on the machine running forge, which trips a
/// microphone check - and a client is usually another machine, whose own
/// devices are the ones it would capture from. So the catalog stays with
/// whoever does the capturing, and what crosses is the input a pick has
/// already moved this process to, in `device` below.
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
    pub pending_ask: Option<PendingAskWire>,
    pub conversation: ConversationWire,
    pub slash_commands: Vec<AvailableCommand>,
    pub subagents: Vec<AvailableAgent>,
    /// Shared with the cache that built it, rather than walked per subscriber:
    /// the walk is a whole tree, and a second client on one seat would pay for
    /// it again. Serialises as the index itself.
    pub file_index: Arc<FileIndex>,
    pub reviews: ReviewsWire,
    /// The working tree behind the git section. The diff itself is a second,
    /// heavier read and is deliberately out of scope.
    pub work: WorkState,
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
}

/// Where a session's reads find their own working tree.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionStateWire {
    pub slot: SessionSlot,
    pub scan_cwd: std::path::PathBuf,
    /// What this session dictates with, where it has overridden the defaults.
    pub dictate_overrides: forge_workspace::DictateOverrides,
}

/// What one seat's composer is doing.
///
/// A client attaching to a running session cannot reconstruct any of it: the
/// stream announces a take, a notice, a compaction and a sign-in once each and
/// retains nothing.
///
/// The ASK the composer is answering rides `pending_ask` on this same record
/// rather than appearing here, because that is the same thing the session's own
/// read answers and two copies of it would drift.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ComposerWire {
    pub take: Option<TakeWire>,
    pub notice: Option<NoticeWire>,
    pub compacting: bool,
    pub sign_in: Option<SignIn>,
}

/// A take in flight, as its meter draws it.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TakeWire {
    pub phase: Phase,
    pub levels: Vec<f32>,
    pub peak_db: f32,
    /// How many segments settled, and the total once the take closed: a live
    /// take cannot know its own total.
    pub progress: (usize, Option<usize>),
    pub floor_db: f32,
    /// How long the take has run. `Instant` is a Rust mechanism and does not
    /// cross, so what crosses is the duration a client draws.
    pub elapsed_ms: u64,
}

/// The session's header facts, including the two a client cannot otherwise
/// reach: the catalogue a picker draws its rows from, and whether a turn is
/// in flight.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionHeaderWire {
    pub model: Option<Value>,
    pub effort: Value,
    pub permission_mode: Option<Value>,
    pub context: Value,
    pub available_models: Vec<Value>,
    pub turn_in_flight: bool,
}

/// What a seat is held on: all three kinds of parked interaction, not the
/// two the dock happened to draw first. Each request crosses as the core's
/// own shape, because a client draws it rather than re-deriving it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum PendingAskWire {
    Permission(Value),
    Question(Value),
    SlackDraft(Value),
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
        }
    }
}

/// The conversation a session's transcript replayed, and how many times it
/// has compacted. The frames cross as the CLI's own messages, which is what
/// the fold reads on both sides.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConversationWire {
    pub messages: Vec<Value>,
    pub compaction_count: u32,
}

/// The review reads for one branch, each carrying its own answer.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReviewsWire {
    pub threads: ReadWire<Vec<ReviewThread>>,
    pub reviews: ReadWire<Vec<ReviewSet>>,
}

/// One page of history: whole turns, newest first, and the handle that asks
/// for the ones above them.
pub struct Page {
    pub rows: Vec<ChatUnit>,
    /// `None` means there is nothing above this page, which is the one case
    /// a client stops asking.
    pub cursor: Option<String>,
}

/// Slice a folded conversation into whole turns, newest first.
///
/// **A turn is a RUN of the fold's units**, so slicing on a unit count would
/// cut a turn in half - the breakage the design exists to avoid. `UserTurn`
/// is a safe boundary because `render_units` flushes any open tool run before
/// it pushes one, so no `UserTurn` can fall inside a `ToolGroup`.
///
/// **The cursor errs toward OVERLAP, never toward a gap.** A client asking
/// for more may be handed a turn it already has - it keys its rows and drops
/// the repeats - but never a HOLE, which is history it has no way to ask for
/// again.
///
/// **The cursor is a POSITION, not a name, and the reason is a fact about the
/// read rather than a preference.** A turn's name would be its
/// `ChatUnit::TurnReport { key }`, and the fold builds a report only from a
/// `Message::Result` frame - while a conversation read from a transcript
/// carries none: `SessionMessageKind` is `User | Assistant | System` with no
/// Result kind, and the replay synthesizer never emits one. So a keyed cursor
/// is `None` on every page of every transcript-derived conversation, and a
/// client reading `None` as "nothing above" stops after the first page. The
/// unit index of the page's first turn is the one thing that always names it.
pub fn page(all: &[ChatUnit], before: Option<&str>, turns: u32) -> Page {
    // A page of no turns ends where it began: its cursor would name the row it
    // already opened at, so a client walking back would ask for the same page
    // forever.
    let turns = turns.max(1) as usize;

    // Where each turn opens. Slicing on a unit count instead would cut a turn
    // in half, because one turn is several units.
    let opens: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, unit)| matches!(unit, ChatUnit::UserTurn { .. }))
        .map(|(at, _)| at)
        .collect();

    // A cursor names the unit the previous page BEGAN at, so the page above
    // ends where that one started: the two meet exactly.
    let ends_at = before
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .and_then(|started| opens.iter().position(|&open| open == started))
        .unwrap_or(opens.len());

    let first = ends_at.saturating_sub(turns);
    // The conversation's opening rows - who started it, a cron fire, a
    // delivery - come before its first turn, so the first page starts at the
    // conversation rather than at that turn. Starting at `opens[0]` would
    // leave them above every page, where no walk can reach them.
    let start = if first == 0 { 0 } else { opens.get(first).copied().unwrap_or(0) };
    let end = opens.get(ends_at).copied().unwrap_or(all.len());
    let rows = all[start..end].to_vec();

    // `None` is the real "nothing above this page": a page already opening on
    // the conversation's first turn has nothing to walk back to, and that is
    // the one case a client stops asking.
    let cursor = if first == 0 { None } else { opens.get(first).map(usize::to_string) };

    Page { rows, cursor }
}

/// A subject's wire form. The ONE place it is produced.
///
/// Async because a session's git section is a filesystem read, so the
/// working tree's state is awaited rather than guessed at.
///
/// A seat forge holds no session for is an error rather than an empty
/// snapshot: `Roster::cwd_for` answers `None` for a project nobody has
/// started, and a client drawing an empty page would read that as a broken
/// one rather than as a seat that does not exist.
pub async fn encode_subject(state: &TransportState, subject: &Subject) -> Result<Value> {
    let surface = &state.surface;
    match subject {
        Subject::Home => Ok(serde_json::to_value(home(state, surface).await)?),
        Subject::Session(slot) => {
            let roster = surface.roster();
            let Some(cwd) = roster.cwd_for(slot) else {
                anyhow::bail!("forge holds no session for {slot:?}");
            };
            walk_processes_if_stale(surface, slot, roster.claude_pid(slot)).await;
            Ok(serde_json::to_value(session(state, surface, slot, &cwd).await?)?)
        }
        // The pool's own report, scanned here rather than carried in another
        // snapshot: it belongs to no seat, and a home snapshot that scanned
        // the pool would pay for the walk on every subscribe.
        Subject::Usage => Ok(serde_json::to_value(surface.usage().await?)?),
    }
}

/// Walk `slot`'s process tree when the snapshot the core holds is missing or
/// older than [`SCAN_STALENESS`], and store what the walk found.
///
/// A view cannot take this walk itself: it shells out to the OS while the
/// surface's reads are synchronous, so a walk on that path would block a
/// render. The socket takes it on the reads that encode a subject instead, and
/// writes through the same store the terminal writes through, so a client
/// built after the terminal is gone still finds a snapshot rather than a seat
/// nothing ever walked.
///
/// The session's live backgrounded `local_bash` commands ride along, from the
/// registry the core holds: a backgrounded bash is `setsid`-detached and sits
/// outside claude's tree, so without them the walk misses the very processes
/// the feed leads with - and a client, having no terminal to ask, would draw a
/// feed the terminal never draws.
pub(crate) async fn walk_processes_if_stale(
    surface: &ViewSurface,
    slot: &SessionSlot,
    pid: Option<u32>,
) {
    let Some(pid) = pid else {
        return;
    };
    let stale = surface
        .processes(slot)
        .is_none_or(|held| held.scanned_at.elapsed().is_ok_and(|age| age >= SCAN_STALENESS));
    if !stale {
        return;
    }
    let commands = local_bash_commands(&surface.background_tasks(slot));
    // `sysinfo`'s refresh is a CPU-bound system call rather than async I/O, so
    // it runs on the blocking pool instead of on this task.
    match tokio::task::spawn_blocking(move || scan(pid, &commands)).await {
        Ok(snapshot) => surface.store_process_snapshot(slot, Some(snapshot)),
        Err(error) => tracing::warn!(
            event_name = "process_walk_failed",
            %error,
            slot = %slot.display(),
            "the process walk did not finish; the seat keeps the snapshot it had",
        ),
    }
}

/// The commands a walk hands the OS scan: each running `local_bash` task's own
/// command.
///
/// A task whose card forge has not seen carries no command and is skipped -
/// the terminal's own rule, where a rostered bash with no recorded command
/// draws its registry row instead of being adopted.
fn local_bash_commands(tasks: &[forge_workspace::BackgroundTask]) -> Vec<String> {
    tasks
        .iter()
        .filter(|task| task.task_type == "local_bash")
        .filter_map(|task| task.command.clone())
        .collect()
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
        projects.push(ProjectWire {
            work: state.work.snapshot(&seat, &project.path).await,
            tasks: roster.tasks_for_project(&project.name),
            crons: roster.crons_for_project(&project.name),
            would_bind: roster.would_bind(&project.key),
            chip: roster.chip_for(&project.key),
            project: project.clone(),
        });
    }

    // Each seat's own tree, read at the directory `cwd_for` resolves FOR IT:
    // the project's path for a lead, the worktree for a git worker. A seat
    // forge holds no directory for keeps `None` rather than borrowing the
    // project's read, which is what the cell's blank has to mean.
    let mut agent_rows = Vec::with_capacity(agents.all().len());
    for agent in agents.all() {
        let work = match roster.cwd_for(&agent.slot) {
            Some(cwd) => Some(state.work.snapshot(&agent.slot, cwd.as_path()).await),
            None => None,
        };
        agent_rows.push(AgentWire { work, ..AgentWire::from(agent) });
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
            gotify: GotifyWire {
                connected: connectors.gotify.connected,
                subscriptions: connectors.gotify.subscriptions,
            },
            slack: SlackWire {
                connected_workspaces: connectors.slack.connected_workspaces.into_iter().collect(),
                load_failed: connectors.slack.load_failed,
                subscriptions: connectors.slack.subscriptions,
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
        unseen: {
            let live = crate::live::Live::lock(&state.live).snapshot();
            agents
                .all()
                .iter()
                .map(|row| row.slot.clone())
                .filter(|slot| live.unseen.is_unseen(slot))
                .collect()
        },
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
    let conversation = {
        let reader = Arc::clone(&state.surface);
        let (seat, root) = (slot.clone(), cwd.to_path_buf());
        tokio::task::spawn_blocking(move || reader.conversation(&seat, &root)).await.unwrap_or_else(
            |error| {
                tracing::warn!(
                    event_name = "conversation_read_failed",
                    %error,
                    slot = %slot.display(),
                    "the transcript read did not finish; the record is answered without it",
                );
                ConversationHistory::default()
            },
        )
    };
    // ONE scan for the working tree, its branch, its count and its PR: the four
    // are then the same instant, so a branch switch between two reads cannot
    // render a PR for a branch this record does not name. It is the scan the
    // terminal's inspector draws the same section from.
    let diff = state.work.diff(slot, cwd).await;
    let work = work_from_scan(&diff, cwd);
    let branch = work.branch.clone().unwrap_or_default();
    let reviews = surface.reviews(slot.project(), &branch);
    let state_at = surface.session(slot, cwd);

    Ok(SessionWire {
        slot: slot.clone(),
        // Through the shared cache rather than the surface's own walk: the
        // walk is a full tree, and every client on this seat would otherwise
        // pay for it again.
        file_index: state.work.files(&state.surface, slot, &state_at.scan_cwd).await,
        slash_commands: surface.slash_commands(slot),
        subagents: surface.subagents(slot),
        mcp: surface.mcp_servers(slot),
        processes: surface.processes(slot),
        background_tasks: surface.background_tasks(slot),
        monitors: surface.monitors(slot),
        pending_ask: surface.pending_ask(slot).as_ref().map(PendingAskWire::from),
        conversation: ConversationWire {
            messages: conversation
                .messages
                .iter()
                .filter_map(|message| serde_json::to_value(message).ok())
                .collect(),
            compaction_count: conversation.compaction_count,
        },
        header: SessionHeaderWire {
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
        },
        composer: {
            let live = crate::live::Live::lock(&state.live).snapshot();
            let composer = &live.composer;
            ComposerWire {
                take: composer.take(slot).map(|take| TakeWire {
                    phase: take.phase,
                    levels: take.levels.iter().copied().collect(),
                    peak_db: take.peak_db,
                    progress: take.progress,
                    floor_db: take.floor_db(),
                    elapsed_ms: u64::try_from(take.elapsed().as_millis()).unwrap_or(u64::MAX),
                }),
                notice: composer.notice(slot).map(NoticeWire::from),
                compacting: composer.compacting(slot),
                sign_in: composer.sign_in(slot).cloned(),
            }
        },
        work,
        pr: diff.pr,
        closes: diff.closes,
    })
}

#[cfg(test)]
mod tests {
    use crate::transcript::ChatUnit;

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

    /// A seat whose transcript holds `turns` finished turns, and the surface
    /// over it.
    fn a_surface_of_turns(turns: usize) -> (Arc<ViewSurface>, SessionSlot, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        let rows: Vec<String> = (0..turns).map(a_turns_rows).collect();
        let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
        fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");

        let seat = fixture_seat();
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");
        (surface, seat, cwd)
    }

    /// A page of no turns ended where it began - an empty page whose cursor
    /// named the row it had already opened at - so a client walking back asked
    /// for it forever.
    #[test]
    fn a_page_of_no_turns_still_walks_backwards() {
        let (surface, seat, cwd) = a_surface_of_turns(6);
        let all = surface.folded_units(&seat, &cwd);

        // Reached the way a client reaches one, from the cursor below it.
        let lower = page(&all, None, 2);
        let cursor = lower.cursor.expect("there is a page above this one");

        let above = page(&all, Some(&cursor), 0);

        assert!(!above.rows.is_empty(), "a page carries rows rather than none at all");
        assert_ne!(
            above.cursor.as_deref(),
            Some(cursor.as_str()),
            "and it moves rather than naming the row it already opened at",
        );
    }

    /// The conversation's opening rows come before its first turn, so a page
    /// starting at that turn leaves them above every page, where no walk
    /// reaches them.
    #[test]
    fn the_rows_before_the_first_turn_ride_the_first_page() {
        let units = [
            ChatUnit::Notice(crate::transcript::Notice {
                severity: crate::transcript::NoticeSeverity::Info,
                source: "cron",
                text: "a scheduled prompt".to_owned(),
            }),
            ChatUnit::UserTurn { text: "hello".to_owned() },
        ];

        let first = page(&units, None, 10);

        assert!(
            matches!(first.rows.first(), Some(ChatUnit::Notice(_))),
            "the opening row is on the page rather than above every page: {:?}",
            first.rows,
        );
    }

    /// Review Focus item 1: a page opens on a turn, and consecutive pages
    /// meet without a gap.
    #[test]
    fn a_page_opens_on_a_turn_and_the_pages_meet_exactly() {
        let (surface, seat, cwd) = a_surface_of_turns(50);
        let all = surface.folded_units(&seat, &cwd);
        let first = page(&all, None, 10);

        // A turn opens on the row the user wrote. A page beginning anywhere
        // else hands a client the tail of one turn and no way to tell that is
        // what it has.
        assert!(
            matches!(first.rows.first(), Some(ChatUnit::UserTurn { .. })),
            "a page opens on a turn rather than inside one",
        );

        // And the next page must reach back to where this one began. It may
        // repeat rows - the client keys them and drops the repeats - but it
        // may never skip one, because a skipped turn is history the reader
        // has no way to ask for again.
        //
        // The evidence is the turn TEXTS rather than a position: comparing
        // positions by pointer is vacuous here, because `page` hands back
        // clones and no clone is ever `ptr::eq` to the original - both sides
        // read as "not found" and the comparison passes whatever happened.
        let written = |page: &Page| -> Vec<String> {
            page.rows
                .iter()
                .filter_map(|unit| match unit {
                    ChatUnit::UserTurn { text } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };
        let all_turns: Vec<String> = written(&Page { rows: all.clone(), cursor: None });
        let second = page(&all, first.cursor.as_deref(), 10);
        let first_turns = written(&first);
        let second_turns = written(&second);

        assert!(!second_turns.is_empty(), "asking for more turns returns some");
        let above = all_turns
            .iter()
            .position(|held| *held == first_turns[0])
            .and_then(|at| at.checked_sub(1))
            .map(|at| all_turns[at].clone());
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
        let (surface, seat, cwd) = a_surface_of_turns(25);
        let all = surface.folded_units(&seat, &cwd);
        let written = |rows: &[ChatUnit]| -> Vec<String> {
            rows.iter()
                .filter_map(|unit| match unit {
                    ChatUnit::UserTurn { text } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };
        let every = written(&all);

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let asked = page(&all, cursor.as_deref(), 5);
            pages += 1;
            assert!(pages < every.len() + 2, "the walk terminates rather than cycling");
            seen.extend(written(&asked.rows));
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

    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::surface::PendingKind;
    use crate::testing::ViewFacts;
    use crate::work::WorkCache;
    use forge_primitives::git_diff::GitDiffSnapshot;

    /// Where the fixture fleet is built. Fixed rather than per-run, so the
    /// fixture does not pin one machine's temp directory.
    const FIXTURE_ROOT: &str = "/tmp/forge-wire-fixture";

    /// The instant a seeded failure counter moved. Fixed rather than taken
    /// from the clock: a fixture pins shape, and a wall-clock stamp would pin
    /// the moment the fixture was generated.
    fn fixture_failure_at() -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
    }

    /// The surface the fixtures are produced from, and both halves of the
    /// reason are deliberate.
    ///
    /// DETERMINISTIC, because the wire carries absolute paths and a fleet
    /// built in a per-run directory would pin the directory rather than the
    /// shape. POPULATED, because an empty fleet pins almost nothing: most of
    /// a session's fields would be `null`, and a field renamed to `null`
    /// would still pass.
    fn fixture_state() -> TransportState {
        let root = Path::new(FIXTURE_ROOT);
        let _ = std::fs::remove_dir_all(root);
        let fleet =
            crate::testing::Fleet::in_dir(root, &[("TestOrg", &["proj"])]).expect("the fleet");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.set_cli_version(Some("1.0.0"), Some("1.1.0"));
        fleet.set_user_preferences(serde_json::json!({}));
        fleet.seed_peer_stats(
            &fixture_seat(),
            forge_primitives::PeerInflightStats { outgoing: 2, incoming: 1, delivery_failed: 3 },
        );
        fleet.seed_peer_failure_at(&fixture_seat(), fixture_failure_at());
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
                    r#"{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
                ],
            )
            .expect("the transcript seeds");
        fleet.seed_test_pending_interaction(&fixture_seat(), PendingKind::Permission);
        // A rostered background task, so the fixture carries a populated
        // registry rather than an empty list a reader cannot tell from an
        // unwired field.
        fleet.seed_view_facts(
            &fixture_seat(),
            ViewFacts {
                background_tasks: vec![forge_workspace::BackgroundTask {
                    task_id: "t-fixture".to_owned(),
                    task_type: "local_bash".to_owned(),
                    description: "gh run watch".to_owned(),
                    command: Some("gh run watch 123 --exit-status".to_owned()),
                }],
                ..ViewFacts::default()
            },
        );

        // A scan the fixture pins the working tree and the PR row from: the
        // fleet's own directory is not a git repository, so without it both
        // fields would be pinned as null and a read that answered nothing at
        // all would round-trip.
        let surface = fleet.surface();
        let work = Arc::new(WorkCache::new());
        let cwd = surface.roster().cwd_for(&fixture_seat()).expect("the fixture seat's directory");
        work.seed_test_diff(&fixture_seat(), &cwd, &scanned());

        TransportState {
            surface,
            work,
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
        }
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
        let state = fixture_state();
        let forms = volatile(Path::new(FIXTURE_ROOT));
        std::fs::create_dir_all(fixtures()[0].1.parent().expect("a directory")).expect("mkdir");
        for (subject, fixture) in fixtures() {
            let mut encoded = encode_subject(&state, &subject).await.expect("encode");
            normalise(&mut encoded, &forms);
            std::fs::write(&fixture, serde_json::to_string_pretty(&encoded).expect("render"))
                .expect("write");
        }
    }

    /// A row's peer-activity badge: the counters the terminal draws per seat.
    ///
    /// They reach a subscriber as an update, so the home's snapshot is what a
    /// client that attached after the last ask reads them from - and a
    /// subscriber that hears the update and not the snapshot draws the badge
    /// from nothing.
    #[tokio::test]
    async fn a_home_agents_row_carries_its_peer_counters() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.seed_peer_stats(
            &fixture_seat(),
            forge_primitives::PeerInflightStats { outgoing: 2, incoming: 1, delivery_failed: 1 },
        );
        fleet.seed_peer_failure_at(&fixture_seat(), fixture_failure_at());
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
        };

        let encoded = encode_subject(&state, &Subject::Home).await.expect("encode");
        let row = encoded["agents"]
            .as_array()
            .expect("agents is a list")
            .iter()
            .find(|row| row["slot"]["label"] == "lead")
            .expect("the started project's lead is a row");

        assert_eq!(
            row["peer"]["outgoing"], 2,
            "the counters the badge draws cross on the row: {row}"
        );
        assert_eq!(
            row["peer_failure_at"]["secs_since_epoch"], 1_700_000_000,
            "and with them the instant the failure counter moved, which is what lets a view age \
             the mark out rather than drawing one the terminal dropped: {row}"
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
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.seed_view_facts(
            &fixture_seat(),
            ViewFacts {
                background_tasks: vec![forge_workspace::BackgroundTask {
                    task_id: "t1".to_owned(),
                    task_type: "local_bash".to_owned(),
                    description: "gh run watch".to_owned(),
                    command: Some("gh run watch 123 --exit-status".to_owned()),
                }],
                ..ViewFacts::default()
            },
        );
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
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

    /// The commands a walk hands the OS scan: the running bash tasks, whose
    /// detached processes sit outside claude's tree, and not the agent tasks
    /// beside them, which the walk never adopts. A task whose card forge has
    /// not seen has no command and is left to the registry's own row.
    #[test]
    fn only_local_bash_tasks_hand_the_scan_a_command() {
        let tasks = vec![
            forge_workspace::BackgroundTask {
                task_id: "t1".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "gh run watch".to_owned(),
                command: Some("gh run watch 123 --exit-status".to_owned()),
            },
            forge_workspace::BackgroundTask {
                task_id: "t2".to_owned(),
                task_type: "local_agent".to_owned(),
                description: "a sub-agent".to_owned(),
                command: None,
            },
            forge_workspace::BackgroundTask {
                task_id: "t3".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "a bash whose card forge never saw".to_owned(),
                command: None,
            },
        ];

        assert_eq!(
            local_bash_commands(&tasks),
            vec!["gh run watch 123 --exit-status".to_owned()],
            "only a bash task's own command is handed to the walk",
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
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
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

    /// A POPULATED PR row, from a seeded scan rather than from `gh`.
    ///
    /// The key's presence is not enough on its own: a regression that answers
    /// no PR - a wrong directory handed to the read, say - keeps the key and
    /// passes that. This drives the values a client draws.
    #[tokio::test]
    async fn the_pr_row_carries_the_scans_pr_and_its_closing_issues() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let seat = fixture_seat();
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");
        let work = Arc::new(WorkCache::new());
        work.seed_test_diff(&seat, &cwd, &scanned());
        let state = TransportState {
            surface,
            work,
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
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

    /// A scan as the terminal's inspector reads it: one branch, one worktree
    /// layer, one PR and one closing issue.
    fn scanned() -> GitDiffSnapshot {
        use forge_primitives::git::{GitBranch, GitIssueRef, GitPrInfo};
        use forge_primitives::git_diff::{GitDiffStats, LayerState, RepoGate};
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
            branch_ahead: LayerState::Clean,
            pr: Some(GitPrInfo { number: 1249, url: "https://example.test/pull/1249".to_owned() }),
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
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
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
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
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

    #[tokio::test]
    async fn every_subject_round_trips_through_its_fixture() {
        let state = fixture_state();
        let forms = volatile(Path::new(FIXTURE_ROOT));

        for (subject, fixture) in fixtures() {
            let mut encoded = encode_subject(&state, &subject).await.expect("encode");
            normalise(&mut encoded, &forms);
            let expected: Value =
                serde_json::from_str(&std::fs::read_to_string(&fixture).expect("read"))
                    .expect("parse");
            assert_eq!(encoded, expected, "{} changed shape", fixture.display());
        }
    }

    /// The walk nothing but the terminal used to take.
    ///
    /// A client built after the terminal is gone would draw PROCESSES empty
    /// forever: no message in this protocol ever produced a snapshot, so the
    /// field read `null` for every seat no terminal happened to be watching.
    /// The socket walks it on the reads that encode a subject.
    #[tokio::test]
    async fn a_stale_process_snapshot_is_walked_and_stored_where_both_readers_find_it() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        let seat = fixture_seat();
        let stale = SystemTime::now() - Duration::from_secs(60);
        fleet.seed_view_facts(
            &seat,
            ViewFacts {
                process_snapshot: Some(ProcessSnapshot {
                    processes: Vec::new(),
                    scanned_at: stale,
                }),
                ..ViewFacts::default()
            },
        );
        let surface = fleet.surface();
        assert_eq!(
            surface.processes(&seat).map(|held| held.scanned_at),
            Some(stale),
            "the fixture holds a snapshot the window has expired",
        );

        walk_processes_if_stale(&surface, &seat, Some(std::process::id())).await;

        let walked = surface.processes(&seat).expect("the walk stored a snapshot");
        assert!(
            walked.scanned_at > stale,
            "a snapshot past the window is walked again, and stored through the store the terminal writes",
        );
    }

    /// The window. A client that re-reads a seat in a loop must not be able to
    /// make the socket walk more often than the terminal's own cadence does.
    #[tokio::test]
    async fn a_fresh_process_snapshot_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        let seat = fixture_seat();
        let fresh = SystemTime::now();
        fleet.seed_view_facts(
            &seat,
            ViewFacts {
                process_snapshot: Some(ProcessSnapshot {
                    processes: Vec::new(),
                    scanned_at: fresh,
                }),
                ..ViewFacts::default()
            },
        );
        let surface = fleet.surface();

        walk_processes_if_stale(&surface, &seat, Some(std::process::id())).await;

        assert_eq!(
            surface.processes(&seat).map(|held| held.scanned_at),
            Some(fresh),
            "a snapshot inside the window is the answer rather than a reason to walk",
        );
    }

    /// The handle a client needs to identify a call. `Task` is the case that
    /// shows why the leaf carries both: its row draws the word `Subagent`,
    /// and that word leads back to no tool, so a client handed only the label
    /// can draw the card and cannot say which call it is.
    #[tokio::test]
    async fn a_groups_leaves_carry_the_tools_name_beside_its_label() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &[
                    r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"go"},"session_id":"s"}"#,
                    r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"tu1","name":"Task","input":{"description":"investigate","prompt":"look"}}]}}"#,
                    r#"{"type":"result","uuid":"r1","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
                ],
            )
            .expect("the transcript seeds");
        let surface = fleet.surface();
        let seat = fixture_seat();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");

        let all = surface.folded_units(&seat, &cwd);
        let leaf = all
            .iter()
            .find_map(|unit| match unit {
                ChatUnit::ToolGroup { families, .. } => {
                    families.iter().flat_map(|family| family.calls.iter()).next()
                }
                _ => None,
            })
            .expect("the fold produced a group with a call in it");

        assert_eq!(leaf.name, "Task", "the leaf names the tool the CLI ran");
        assert_eq!(
            leaf.label, "Subagent",
            "and the word its row draws is a different thing, which is why both are carried",
        );
    }

    /// A client could set a session's dictation and move the process's input,
    /// and no record carried either back: three commands crossed the wire and
    /// nothing read what they had set.
    #[tokio::test]
    async fn the_dictation_a_client_set_comes_back() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
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
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
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
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
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
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.add_cron("proj", "a nightly sweep").expect("the cron is added");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
        };

        let encoded = encode_subject(&state, &Subject::Home).await.expect("encode");
        let rows = encoded["projects"].as_array().expect("the home carries project rows");
        let crons = rows[0]["crons"].as_array().expect("and each row carries its schedules");

        assert_eq!(crons.len(), 1, "the project's own cron reached the wire: {crons:?}");
        assert_eq!(crons[0]["prompt"], "a nightly sweep");
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
        let dir = tempfile::tempdir().expect("tempdir").keep();
        // The trees go up BEFORE the fleet does. A project's registry key is
        // derived from its path and the derivation canonicalises, so a key
        // taken while the directory is absent is not the key taken once it is
        // there - and a worker registered under the first is registered under
        // one nothing looks up again.
        let root = dir.join("proj");
        let worktree = root.join(".claude/worktrees/w1");
        repo_at(&root, "main", 1);
        repo_at(&worktree, "worktree-em-dash-sweep", 2);

        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.add_git_worker("TestOrg", "proj", "w1").expect("the worker is added");

        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
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
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let root = dir.join("proj");
        repo_at(&root, "main", 1);

        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.add_despawned_worker("proj", "gone").expect("the label is left behind");

        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
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

    /// A seat with nothing behind it has no tree to walk, and what it holds is
    /// kept rather than replaced by an empty walk. An invented empty snapshot
    /// would draw as `no processes` where the truth is `nothing known`.
    #[tokio::test]
    async fn a_seat_with_no_claude_process_keeps_the_snapshot_it_had() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        let seat = fixture_seat();
        let held = SystemTime::now() - Duration::from_secs(60);
        fleet.seed_view_facts(
            &seat,
            ViewFacts {
                process_snapshot: Some(ProcessSnapshot { processes: Vec::new(), scanned_at: held }),
                ..ViewFacts::default()
            },
        );
        let surface = fleet.surface();

        walk_processes_if_stale(&surface, &seat, None).await;

        assert_eq!(
            surface.processes(&seat).map(|snapshot| snapshot.scanned_at),
            Some(held),
            "a seat with no process to walk keeps the snapshot it had rather than inventing an empty one",
        );
    }
}
