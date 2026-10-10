//! The orchestrator.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use forge_agent::AgentHandle;
use forge_agent::client::SessionLaunchSettings;
use forge_agent::env::cli_version::CliVersionInfo;
use forge_primitives::cloud::service_status::ServiceIssue;
use forge_primitives::tasks::TaskId;
use forge_primitives::{AvailableAgent, AvailableCommand, Message, SDKSessionInfo};

use crate::mcp::peers::types::{MessageId, WrappedKind, WrappedPrompt};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::Instrument;

use forge_gateway::AccountKey;
use forge_gateway::ProviderHost as _;

use crate::config::{LoadedConfig, LoadedProject, load_from_dir};
use crate::domain_session::DomainSession;
use crate::error::WorkspaceError;
use crate::protocol::{
    Command, DispatchError, NoticeSeverity, PromptOrigin, PromptSource, SessionUpdate,
};
use crate::session_task::SessionTask;
use crate::spawn;
use crate::target::{ProjectKey, SessionSlot, SessionTarget};
use crate::update_fanout::{SubscriberRole, UpdateFanout};
use crate::views::{AccountLoadingRow, ProjectView, SessionView};

#[cfg(any(test, feature = "testing"))]
pub(crate) mod testing;

/// How often the background poller refreshes account usage. The
/// TUI's bottom panel and the gateway's account selection both read
/// from the cache this poll populates. 60 s upper-bounds how stale
/// the "which account has more headroom" decision can be while
/// staying clear of the OAuth usage endpoint's 429 throttle under
/// multi-instance polling - combined with per-account `last_error`
/// backoff (see `forge_gateway::AccountState`), transient 429s recover
/// naturally.
const USAGE_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How often the claude version probe re-runs after the boot fetch.
/// Versions change rarely, so a few minutes is ample - the point is only
/// to recover a transient boot failure, not to track releases tightly.
const CLI_VERSION_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// The CLI's model-slot variables: the project's `model` key is
/// stamped into all of them at spawn, so one model serves the main
/// loop, subagents and background slots alike.
const MODEL_SLOT_VARIABLES: [&str; 5] = [
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

/// Max attempts for `tag_session_with_retry` to find the worker's
/// `<session_id>.jsonl` and append the tag row. claude CLI writes the
/// file lazily on the first user turn - workers with an `initial_prompt`
/// usually land within ~100 ms-2 s of `Connected`. 30 attempts at
/// 100 ms = ~3 s wall caps the window generously without burning
/// resources. Idle-spawned workers (no prompt) never produce a JSONL
/// at all until the first `DeliverWorkerPrompt`; for those we exit
/// the retry with `Io(NotFound)`, mark the entry `needs_tag = true`,
/// and retry opportunistically when the first turn arrives.
const WORKER_TAG_RETRY_ATTEMPTS: u32 = 30;

/// Delay between `tag_session` retry attempts when the JSONL is not
/// yet on disk. 100 ms is short enough to land within one or two ticks
/// of claude's first write and large enough that 30 retries cap the
/// total wait at a few seconds.
const WORKER_TAG_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Minimum gap between successive worker-kick dispatches (#259).
/// Multi-worker teams hit Anthropic's per-IP burst limit when all
/// `maybe_kick_worker_on_connected` paths fire `Command::Prompt`
/// within the same tick at boot; routing every kick through a
/// workspace-level mpsc + a drainer task that sleeps this interval
/// between sends spreads them out enough to avoid the burst
/// rejection. The first kick in an empty channel fires with zero
/// added latency; only subsequent kicks pay the sleep. Worst case
/// at typical team sizes (7-10 workers) is ~5-7 s to fully kick
/// the cohort, vs ~1 s pre-#259 with most kicks rejected.
const KICK_DISPATCH_INTERVAL: Duration = Duration::from_millis(750);

/// How often the auto-continue sweep looks for a nudge whose delay has run
/// out. The delay is what a reader races; this only rounds it.
const AUTO_CONTINUE_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The words a nudged seat's model receives, fixed by the issue.
fn auto_continue_prompt(reason: &str) -> String {
    format!("The previous turn failed: {reason}. Continue from where you left off.")
}

/// Delegation block appended to a Lead session's system prompt.
///
/// Lead-only: `agents__spawn` refuses a worker caller, so a worker
/// given this block would be told to call a tool that rejects it.
///
/// Matches the shipped-prompt constants in `forge-agent`: one escaped
/// literal, no runtime assembly.
const LEAD_DELEGATION_PREAMBLE: &str = "\
You can delegate work to worker sessions via the \
mcp__forge__agents__ tools. Spawn one with \
agents__spawn(label=\"<name>\", charter=\"<its mission>\") - the charter \
is required and is what defines that worker; talk to it with \
agents__send_message; list your live workers with agents__list; \
revise a worker's stored charter or kicks with agents__update, which \
takes effect on its next restart. agents__spawn always creates the \
worker in YOUR project. The same family reaches other projects' \
agents: a message takes an org and project, plus a label to name a \
worker rather than that project's own agent. At most one live worker \
exists per label - if it already exists, message it instead of \
spawning again. \
Spawned workers are durable: they survive forge restarts and re-spawn \
automatically, resuming where they left off, until you explicitly \
despawn them with agents__despawn (or close their row in the Projects \
pane). Despawn a worker once its work is truly done, otherwise it keeps \
coming back on every restart. A PR review loop \
fans out as ephemeral in-session subagents, not workers - a reviewer \
spawned as a worker lingers as a durable row and worktree after its \
round ends. Workers build; subagents review, unless the user wants a \
reviewer kept on as a long-lived worker.";

/// Forge-supplied resume kick for a worker whose row carries no
/// `resume_kick` of its own. On a resuming re-spawn forge delivers this
/// constant as the worker's first turn - telling it to continue rather
/// than start the task over.
const DYNAMIC_WORKER_RESTART_NOTE: &str = "This session was restarted by forge. Your prior conversation and progress are in the history above. Continue where you left off; do not restart the task.";

/// One enqueue onto the workspace-level worker-kick channel
/// (#259). Built by `maybe_kick_worker_on_connected` (and any
/// future kick site); drained by the workspace's
/// `start_kick_dispatcher` task, which fires one `Command::Prompt`
/// per `KICK_DISPATCH_INTERVAL` tick. Same payload shape as the
/// existing `Command::Prompt` carries (no attachments are ever
/// part of a kick prompt - kicks are pure text).
#[derive(Debug)]
pub(crate) struct KickRequest {
    pub slot: SessionSlot,
    pub prompt_body: String,
}

/// Per-session chip the Projects pane renders next to each row.
/// Carries the assigned account display name + the visual-state
/// category derived by `Workspace::session_chip_for`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionChipInfo {
    /// Account `display_name` from forge.toml `[[accounts]]`.
    pub account_name: String,
    /// Render category: drives the chip's color + optional prefix
    /// glyph.
    pub state: SessionChipState,
}

/// Visual category for a session chip. The renderer maps these to
/// foreground colors + (for `Bailed` alone) a leading `⚠ ` glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionChipState {
    /// Account is Ready and within budget. DIM foreground.
    Normal,
    /// Account is Ready but at least one usage window (5h or weekly)
    /// is currently at the cap. Yellow foreground signals "still
    /// spawns but expect throttling until the window resets." When
    /// every account that declares the model is capped the walk takes
    /// one anyway, so the chip shows this state.
    AtCap,
    /// Account Bailed on an auth failure (rejected or expired
    /// credentials). Red foreground + `⚠ ` prefix; the repair is an
    /// env edit plus a restart.
    Bailed,
    /// Account Bailed on a transient failure (rate limit, unreachable
    /// endpoint, malformed response). Warning yellow, no glyph - the
    /// same split the account rows render; the pollers heal it.
    Degraded,
}

/// What a session is waiting on a person for. The kind a needs-you row
/// names, since "asked you a question" and "a permission prompt is
/// waiting" are different asks with the same mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingInteractionKind {
    Question,
    Permission,
}

/// The dedupe key for a delivered Slack message: one destination is
/// (project, owner, conversation, ts), so a lead and a worker in one
/// project never starve each other as "already delivered".
pub(crate) type SlackDeliveryKey = (String, Option<String>, String, String);

/// Multi-session orchestrator. Owns the project catalog snapshot
/// loaded from `<config_dir>/forge.toml` and the pool of currently
/// One composed Slack message held for a decision: the session that asked,
/// the draft itself, and the sender the blocked `slack__post` handler awaits.
///
/// The draft rides beside the sender for the same reason the permission and
/// question requests do - the stream carries it once, so a view that attached
/// after it landed has nothing else to draw the dock from.
pub(crate) type ParkedSlackDraft =
    (SessionSlot, forge_primitives::slack::SlackDraft, tokio::sync::oneshot::Sender<bool>);

/// One parked browser hand-off: the session that asked, the hand-off itself,
/// and the sender the blocked `browser_hand_off` handler awaits.
///
/// It rides beside the sender for the same reason the Slack draft does - the
/// update carries it once, so a view that attached after it landed has nothing
/// else to draw the dock from - and the sender is what makes the wait end: no
/// clock ends it, only an answer, or the handler's own drop.
pub(crate) type ParkedBrowserHandOff = (
    SessionSlot,
    forge_primitives::browser::HandOff,
    tokio::sync::oneshot::Sender<forge_primitives::browser::HandOffEnding>,
);

/// What the last spawn handed `Agent::spawn` as its listing.
///
/// Three cases, because a lead's listing and a spawn that never ran are
/// different answers and one option would merge them. Test-only: a
/// stand-in replaces the spawn call outright, so this is the only way a
/// test sees what the real spawn was given.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Clone)]
pub enum RecordedListing {
    /// No spawn has run since the workspace was built.
    None,
    /// A spawn ran and handed no listing: a lead's own case.
    NoListing,
    /// A spawn ran and handed this worker's listing.
    Listed(forge_agent::WorkerListing),
}

/// spawned [`forge_agent::Agent`] handles, one per active session.
///
/// Construct via [`Workspace::new`]; consume via
/// [`Workspace::get_agent_handle`]; drain on exit via
/// [`Workspace::shutdown`].
pub struct Workspace {
    /// Retained so error messages can reference the `forge.toml`
    /// path the workspace was constructed from, and so the per-account
    /// config-dir binding can re-resolve from here.
    config_dir: PathBuf,
    /// `pub(crate)` so the impl block in [`crate::gotify`] can read the
    /// `[gotify]` section.
    pub(crate) config: LoadedConfig,
    /// The `[systemone]` decision client, built at boot when the section
    /// is present and enabled; the session MCP servers are handed a
    /// facade over it. `None` keeps the systemone tools out of every
    /// session.
    pub(crate) systemone: Option<Arc<forge_system_one::SystemOneClient>>,
    /// Catalog of sessions per project. Populated by the background
    /// catalog scan once [`Workspace::start_catalog_scan`] runs;
    /// mutated in-place by [`Workspace::record_connected_session`]
    /// each time a freshly spawned session reaches `Connected`, so the
    /// Projects pane's drilldown stays current without forcing a full
    /// disk re-scan. Held under a Mutex because multiple in-process
    /// tasks (the pane render, the connect-flow event handler) reach
    /// for it across `await` points; `Arc` so the scan task can swap
    /// its contents without holding an `Arc<Workspace>` cycle.
    catalog: Arc<Mutex<HashMap<ProjectKey, Vec<SDKSessionInfo>>>>,
    /// Live Agents keyed by the session slot. `parking_lot::Mutex` so the
    /// public methods can take `&self`. `pub(crate)` so sibling
    /// modules (`spawn::handle_deliver_worker_prompt_to_lead`,
    /// `mcp::workers::facade::ProdWorkerFacade`) can probe pool
    /// membership for lead-delivery gating without an extra method
    /// wrapper.
    pub(crate) pool: Mutex<HashMap<SessionSlot, PooledAgent>>,
    /// A stand-in for the handle a cold spawn would create. The settings a
    /// spawn launches with are otherwise unobservable: a pooled slot never
    /// reaches the launch, and a cold one ends in a real subprocess. `None`
    /// in production and in every test that does not install one, so a
    /// spawn runs always.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) test_spawn_handle: Mutex<Option<forge_agent::AgentHandle>>,
    /// The worker listing the last spawn handed `Agent::spawn`, written by
    /// the spawn and read by a test that cannot see the call: a stand-in
    /// replaces `Agent::spawn` entirely, so its argument is otherwise
    /// unobservable.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) test_spawn_listing: Mutex<RecordedListing>,
    /// The `forge` MCP server the last spawn built, for the same reason the
    /// listing above is kept: a stand-in replaces `Agent::spawn`, so the
    /// server it would have been handed is otherwise dropped unobserved -
    /// and a test that has to drive a tool through the spawn's OWN
    /// composition (which facade it was built with) needs it.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) test_spawn_server: Mutex<Option<forge_sdk::mcp::server::McpServer>>,
    /// The account state map, owned by the gateway and reached through
    /// its pool. It carries account health state updated on every spawn
    /// and refreshed by the in-memory usage poller, and it is what the
    /// gateway's selection walk reads to choose an account.
    accounts: Arc<forge_gateway::AccountPool>,
    /// Dictation preflight: the per-model progress the launchpad
    /// renders, the flag Escape sets, and the loaded engine held for
    /// the run. Populated by `start_dictate_preflight`; inert when
    /// `[dictate] enabled` is false.
    pub(crate) dictate: Arc<crate::dictate::DictateState>,
    /// Live dictation: the recording holding the microphone and the
    /// submitted takes still awaiting a transcript. Driven by
    /// `Command::DictateStart` / `Command::DictateStop`.
    pub(crate) dictate_runtime: Mutex<crate::dictate::DictateRuntime>,
    /// The `/dictate` overlay's input-device pick, shared by every
    /// session: `None` until one lands, then it overrides the
    /// `forge.toml` `[dictate] device` pin for every capture until the
    /// process ends. Set by `Command::SetDictateDevice`; a Reset
    /// clears it. Volatile, never persisted.
    pub(crate) dictate_device_pick: Mutex<Option<crate::dictate::DictateDeviceChoice>>,
    /// The browser relay: the one client connection that drives the browser.
    /// The workspace owns it because both halves need the same one - the
    /// tools in `mcp::browser` ask through it, and the transport registers
    /// the connection that answers into it.
    pub(crate) browser: Arc<crate::browser::BrowserRelay>,
    /// The model catalogue: the last fetched feed and what the last check
    /// did. Loaded from its cache at boot, refreshed by
    /// `start_dictate_catalogue` and `Command::DictateCatalogueCheck`, and
    /// read by the models page through `dictate_models`.
    pub(crate) dictate_catalogue: Mutex<crate::catalogue::CatalogueState>,
    /// Where the last model install got to. Set by
    /// `Command::DictateInstall`, read by the models page through
    /// `dictate_install`.
    pub(crate) dictate_install: Mutex<crate::install::InstallState>,
    /// Where the last model activation got to. Set by
    /// `Command::DictateActivate`, read by the models page through
    /// `dictate_activate`.
    pub(crate) dictate_activate: Mutex<crate::install::ActivateState>,
    /// Where the last bench got to, and the flag its Stop sets. Read by
    /// the models page through `dictate_bench` and `bench_results`.
    pub(crate) dictate_bench: Mutex<crate::bench::BenchState>,
    pub(crate) dictate_bench_cancel: std::sync::atomic::AtomicBool,
    /// The last read-aloud write's failure, when there was one: a stop
    /// whose write fails has no dispatch left to answer, so the page reads
    /// it here instead. Cleared when the next recording starts.
    pub(crate) read_aloud_error: Mutex<Option<String>>,
    /// A test's own catalogue source, so a check can run against a
    /// loopback server instead of GitHub. Not present in production
    /// builds.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) test_catalogue_source: Mutex<Option<forge_dictate::catalogue::CatalogueSource>>,
    /// A test's own cleanup source, on the same terms.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) test_cleanup_source: Mutex<Option<forge_dictate::cleanup::CleanupSource>>,
    /// A test's own catalogue directory, so a check never writes to the
    /// real app-support dir. Not present in production builds.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) test_catalogue_dir: Mutex<Option<std::path::PathBuf>>,
    /// A test's own read-aloud set directory, so an arming never writes to
    /// the real app-support dir. Not present in production builds.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) test_read_aloud_dir: Mutex<Option<std::path::PathBuf>>,
    /// Fan-in [`SessionUpdate`] sender: every producer inside the
    /// workspace holds a clone, and it fans each update out to whatever
    /// subscribed at [`Self::subscribe`].
    /// `pub(crate)` so the impl block in [`crate::crons`] can reach it.
    pub(crate) update_tx: UpdateFanout,
    /// Per-session [`Command`] sender map. Populated when
    /// [`Self::get_agent_handle`] spawns the first `SessionTask` for a
    /// key; cleared on [`Self::release_session_with_cascade`] and [`Self::shutdown`].
    #[cfg(any(test, feature = "testing"))]
    pub(crate) command_senders: Mutex<HashMap<SessionSlot, mpsc::UnboundedSender<Command>>>,
    #[cfg(not(any(test, feature = "testing")))]
    command_senders: Mutex<HashMap<SessionSlot, mpsc::UnboundedSender<Command>>>,
    /// Per-project list of live worker sessions. In-memory only -
    /// wiped on forge restart by design (workers are ephemeral at the
    /// forge UI level; their JSONLs persist on disk). Mutated via
    /// `insert_live_worker` / `remove_latest_worker` / `drain_live_workers`.
    live_workers: Mutex<HashMap<ProjectKey, Vec<crate::mcp::workers::types::WorkerEntry>>>,
    /// The `(project, label)` pairs whose despawn cleanup is still running.
    /// The label's worktree is being torn down for that whole window, so a
    /// spawn taking the label now would enter a directory the removal is
    /// about to delete. Held from the cleanup's handoff until it finishes,
    /// and released by its own `Drop` so a panicking cleanup cannot leave a
    /// label blocked forever. In-memory only, like the rest of the worker
    /// registry.
    despawn_cleanups: Mutex<HashSet<(ProjectKey, String)>>,
    /// Why the last spawn or connection for a slot failed, keyed by slot.
    /// Survives the failed attempt itself, which is what a `DomainSession`
    /// cannot: a connection failure releases the session, so by the time a
    /// view reads the slot there is nothing left to say the attempt ever
    /// happened. Written before that release and cleared when a session
    /// for the slot comes up again, so it reports an unrecovered failure
    /// rather than a history.
    spawn_failures: Mutex<HashMap<SessionSlot, String>>,
    /// The seats a view is showing, each with the working-tree scan loop it
    /// runs. Empty is the quiet case: a seat nobody holds is not scanned.
    pub(crate) held_work_seats: crate::work::HeldSeats,
    /// Shared [`DomainSession`] handles, one per active `SessionTask`.
    /// `pub(crate)` so crate-internal spawn and delivery paths can
    /// reach a session's `DomainSession` directly.
    pub(crate) domain_handles: Mutex<HashMap<SessionSlot, Arc<Mutex<DomainSession>>>>,
    /// The session that submitted the reviews on a `(project, branch)` -
    /// the target for a worker's review-activity notice. Set by
    /// [`Self::submit_review`]; latest submit wins (the reviewer is one
    /// human whose session may rekey across a resume). `pub(crate)` so
    /// the impl block in [`crate::review`] can reach it.
    pub(crate) review_origin: Mutex<HashMap<(String, String), SessionSlot>>,
    /// Review actions a caller took during its current turn, keyed by the
    /// caller's [`SessionSlot`]. Appended by [`Self::review_reply`] /
    /// [`Self::review_resolve`] and drained into one notice per review at
    /// the caller's turn end ([`Self::drain_review_activity`]).
    /// `pub(crate)` so the impl block in [`crate::review`] can reach it.
    pub(crate) review_activity:
        Mutex<HashMap<SessionSlot, Vec<crate::mcp::review::ReviewActivity>>>,
    /// Set the first time [`Self::start_usage_poller`] runs. Subsequent
    /// calls early-return to avoid spawning duplicate poller tasks.
    usage_poller_started: std::sync::atomic::AtomicBool,
    /// The gateway's listener + session bindings. Children are stamped
    /// with a base URL that names this listener, and every spawn
    /// registers here.
    pub(crate) gateway: Arc<forge_gateway::forward::Gateway>,
    /// `true` once the listener's port is bound. The boot gate stays
    /// shut until it is, because every session's base URL names it.
    gateway_ready: std::sync::atomic::AtomicBool,
    /// The port the listener binds: `[gateway] port` or the default.
    gateway_port: u16,
    /// The listener's bound URL, stored when the port is taken. The
    /// stamp reads THIS, not the config value, so the base URL a child
    /// is stamped with is structurally the one that answers.
    gateway_url: Mutex<Option<String>>,
    /// The bind failure, if the listener could not start. Surfaced by
    /// the launchpad's gate label; `None` while loading or bound.
    gateway_bind_error: Mutex<Option<String>>,
    /// Guards against double-spawning the cron scheduler (mirrors
    /// `usage_poller_started`). Started once at boot from the binary.
    /// `pub(crate)` so the impl block in [`crate::crons`] can reach it.
    pub(crate) cron_scheduler_started: std::sync::atomic::AtomicBool,
    /// Guards against double-spawning the chase sweep (mirrors
    /// `cron_scheduler_started`). Started once at boot from the binary.
    /// `pub(crate)` so the impl block in [`crate::chase`] can reach it.
    pub(crate) chase_sweep_started: std::sync::atomic::AtomicBool,
    /// Guards against double-spawning the auto-continue sweep (mirrors
    /// `cron_scheduler_started`). Started once at boot from the binary.
    auto_continue_sweep_started: std::sync::atomic::AtomicBool,
    /// Sender half of the worker-kick channel (#259). Cloned via
    /// [`Self::enqueue_kick`] by `maybe_kick_worker_on_connected`
    /// (and any future kick site). The matching receiver lives in
    /// `kick_dispatcher_rx_slot` until [`Self::start_kick_dispatcher`]
    /// takes it out and spawns the drainer task.
    kick_dispatcher_tx: mpsc::UnboundedSender<KickRequest>,
    /// Single-take slot holding the matching receiver.
    /// [`Self::start_kick_dispatcher`] pops it on first call and
    /// hands it to the drainer task; subsequent calls find `None`
    /// and no-op (mirrors `start_usage_poller`'s guard against
    /// duplicate spawns).
    kick_dispatcher_rx_slot: Mutex<Option<mpsc::UnboundedReceiver<KickRequest>>>,
    /// Single-instance lock file held open for the process lifetime.
    /// `Workspace::new` takes an exclusive flock on a machine-local
    /// lockfile keyed by the config dir (see [`crate::single_instance`])
    /// so a second forge on the same config dir refuses to start; the
    /// flock releases when this `File` drops (Workspace teardown / process
    /// exit / crash). Held purely for that side effect - never read.
    /// `None` in `testing_stub` and on the degraded acquire path.
    _single_instance_lock: Option<std::fs::File>,
    /// Durable forge crons (`mcp__forge__cron`). In-memory working set,
    /// loaded at boot from the machine-local redb store ([`crate::store::cron`])
    /// and persisted back after every mutation - create/delete, the
    /// scheduler's fire-advance, and boot catch-up - through the one
    /// [`Workspace::with_crons_mut`] path. The single-instance guard keeps
    /// this the only process holding that store, so this mutex alone
    /// serialises writes.
    /// `pub(crate)` so the impl block in [`crate::crons`] can reach it.
    pub(crate) crons: Mutex<Vec<forge_primitives::CronEntry>>,
    /// The live task list (`mcp__forge__tasks`). In-memory working set,
    /// loaded from the machine-local store at boot and persisted back
    /// after every mutation through the one [`Workspace::with_tasks_mut`]
    /// path. `pub(crate)` so the impl block in [`crate::tasks`] can reach
    /// it.
    pub(crate) tasks: Mutex<Vec<forge_primitives::tasks::Task>>,
    /// Payloads addressed to a slot that had no live session when they
    /// arrived - a peer prompt, a fired cron, a Gotify notification, a
    /// Slack message - keyed by `(org, project, label)` (`None` = lead),
    /// the triple the `sessions` table uses. The slot's session drains
    /// its own bucket on first `Connected`. `pub(crate)` so the impl
    /// block in [`crate::parked`] can reach it.
    pub(crate) parked_by_slot: Mutex<crate::parked::ParkedMap>,
    /// Active Gotify subscriptions (`mcp__forge__gotify`). The set the
    /// stream matches each inbound message against. Durable ones (lead /
    /// team-worker) are also persisted to `db` and reloaded here
    /// at boot; ephemeral ad-hoc-worker ones live only in memory.
    /// `pub(crate)` so the impl block in [`crate::gotify`] can reach it.
    pub(crate) gotify_subs: Mutex<Vec<forge_primitives::GotifySubscription>>,
    /// Machine-local redb store. Backs durable crons ([`crate::store::cron`]),
    /// Gotify and Slack subscriptions ([`crate::store::gotify`],
    /// [`crate::store::slack`]) and persisted sessions
    /// ([`crate::store::sessions`]). `None` when the DB couldn't
    /// open (degrade to in-memory-only, no persistence) or in
    /// `testing_stub`. `pub(crate)` so the impl block in
    /// [`crate::review`] can reach it.
    pub(crate) db: Arc<Mutex<Option<crate::store::Db>>>,
    /// Whether the boot catalog scan has populated `catalog`. The scan
    /// runs in the background off the boot path; the Projects pane and
    /// the session lookups read the catalog it fills.
    catalog_loaded: Arc<std::sync::atomic::AtomicBool>,
    /// Idempotence guard for [`Workspace::start_catalog_scan`].
    catalog_scan_started: std::sync::atomic::AtomicBool,
    /// The installed and npm-published `claude` CLI versions, merged from
    /// every probe so far. One answer for every viewer, so it lives here
    /// rather than in the view that happened to probe first; `None` until
    /// the boot probe lands.
    cli_version: Arc<Mutex<Option<CliVersionInfo>>>,
    /// The last fatal error, held so a view that was not subscribed when it
    /// fired can still learn of it: the update carries no state of its own
    /// and nothing else in the core records it.
    last_fatal_error: Mutex<Option<forge_primitives::error::AppError>>,
    /// The statuspage's last answer, held so a view that attached after the
    /// probe landed can read it rather than having missed the one update
    /// that carried it. `None` until the probe answers.
    service_status: Arc<Mutex<Option<ServiceIssue>>>,
    /// Idempotence guard for the service-status probe.
    service_status_probe_started: std::sync::atomic::AtomicBool,
    /// Idempotence guard for the claude version probe: a second start
    /// would leave two loops each spawning `claude --version` and reaching
    /// npm.
    cli_version_probe_started: std::sync::atomic::AtomicBool,
    /// Whether the Gotify stream is currently connected. Set by the
    /// subsystem pump on `Connected` / `Disconnected`; read by the
    /// Inspector's status line.
    /// `pub(crate)` so the impl block in [`crate::gotify`] can reach it.
    pub(crate) gotify_connected: Mutex<bool>,
    /// Gotify application name -> numeric appid map, fetched from the
    /// server's `/application` list on subsystem start and refreshed on
    /// each reconnect. Resolves an `application` name filter to the appid
    /// inbound messages carry, and the reverse lookup for the envelope.
    /// `pub(crate)` so the impl block in [`crate::gotify`] can reach it.
    pub(crate) gotify_app_index: Mutex<HashMap<String, u64>>,
    /// Shutdown handle for the running Gotify subsystem. `Some` while the
    /// stream task is live; dropping/sending stops it. `None` when idle
    /// (no subscriptions) or unconfigured. Guards against double-starting.
    /// `pub(crate)` so the impl block in [`crate::gotify`] can reach it.
    pub(crate) gotify_subsystem: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// One Slack client per `[[slack]]` entry, resolved at boot. Empty
    /// keeps the Slack connector dormant. `pub(crate)` so the impl
    /// blocks in [`crate::slack`] and `crate::mcp::slack` can reach it.
    pub(crate) slack: Arc<crate::slack::SlackWorkspaces>,
    /// Active Slack subscriptions (`mcp__forge__slack`). The set each
    /// workspace's pump sweeps. Durable ones are also persisted to `db`
    /// and reloaded here at boot. `pub(crate)` so the impl block in
    /// [`crate::slack`] can reach it.
    pub(crate) slack_subs: Mutex<Vec<forge_primitives::slack::SlackSubscription>>,
    /// Shutdown handles for the running Slack pumps, one per workspace
    /// label. Slack has a pump per `[[slack]]` entry where Gotify has a
    /// single server and a single handle.
    pub(crate) slack_subsystem:
        Mutex<std::collections::BTreeMap<String, tokio::sync::oneshot::Sender<()>>>,
    /// Per-workspace pump liveness for the Inspector's SLACK section.
    /// Keyed by label for the same reason.
    pub(crate) slack_connected: Mutex<std::collections::BTreeMap<String, bool>>,
    /// The authenticated user's id per workspace, resolved once by the
    /// boot `auth.test` and needed to recognise `<@U...>` mentions.
    pub(crate) slack_user_ids: Mutex<std::collections::BTreeMap<String, String>>,
    /// Display names resolved per `(workspace, user id)`, so a message from
    /// a known author costs no `users.info` call.
    pub(crate) slack_user_names: Mutex<std::collections::BTreeMap<(String, String), String>>,
    /// `(workspace, user id)` pairs whose failed lookup has been reported, so
    /// a persistent failure says so once rather than on every tick.
    pub(crate) slack_author_failures: Mutex<std::collections::HashSet<(String, String)>>,
    /// Composed Slack messages held for the user's decision, keyed by
    /// draft id and carrying the session that asked. The owner is stored
    /// beside it so an answer is only ever applied by the session it was
    /// addressed to.
    pub(crate) slack_drafts: Mutex<HashMap<uuid::Uuid, ParkedSlackDraft>>,
    /// Browser hand-offs parked for the person at a client, keyed by their id
    /// and carrying the session that asked. The owner is stored beside each so
    /// an answer is only ever applied by the session it was addressed to; the
    /// blocked `browser_hand_off` handler awaits the sender, with no timeout,
    /// for as long as the person takes.
    pub(crate) browser_handoffs: Mutex<HashMap<uuid::Uuid, ParkedBrowserHandOff>>,
    /// Slack messages handed to a session recently, keyed by
    /// `(project, owner, conversation, ts)`. A sweep re-runs a batch
    /// whenever a 429 lands mid-sweep, a watermark write fails, or the
    /// process dies between the two - this is what makes that re-run
    /// idempotent rather than a re-delivery.
    pub(crate) slack_recently_delivered: Mutex<HashMap<SlackDeliveryKey, std::time::Instant>>,
    /// Set at boot when the durable Slack subscriptions could not be
    /// read. The Inspector SLACK section reads it: without it the empty
    /// subscription set would hide the failure.
    pub(crate) slack_load_failed: std::sync::atomic::AtomicBool,
    /// The last time each workspace's user-id retry ran, so a failing
    /// `auth.test` is retried at most once a minute rather than per sweep.
    pub(crate) slack_user_id_retries: Mutex<std::collections::BTreeMap<String, SystemTime>>,
    /// Set the first time [`Workspace::start_slack_verification`] runs.
    /// Subsequent calls early-return to avoid spawning duplicate probes.
    pub(crate) slack_verification_started: std::sync::atomic::AtomicBool,
    /// Per-project in-flight guard for the lead Connected
    /// hook's catalog scan. Inserted synchronously when
    /// `respawn_workers_for_lead` starts; removed when
    /// the async scan completes and dispatches its SpawnWorker
    /// commands. A concurrent second Connected (e.g. a fast /new
    /// reconnect) checking this set sees the entry and skips its own
    /// respawn, preventing duplicate worker sets while the scan
    /// is in flight. The existing `live_workers.is_empty()` gate
    /// covers the post-dispatch case.
    respawn_in_flight: Mutex<std::collections::HashSet<ProjectKey>>,
    /// The cron ids the LAST `fire_due_crons` pass found unwakeable, so
    /// the first warning about one is a `WARN` and the repeats are not.
    ///
    /// Not a cache: nothing but that warning's level reads it, and it
    /// holds only what the most recent pass saw, replaced wholesale at the
    /// end of each pass. That is what stops an entry outliving its
    /// condition - a cron that fires, advances or is deleted is simply
    /// absent from the next pass's set, so one that goes unwakeable again
    /// is new again and warns at `WARN` again. A set that never cleared
    /// would silence a cron that is firing.
    unwakeable_crons: Mutex<std::collections::HashSet<forge_primitives::CronId>>,
    /// Test-only intercept buffer for app-level Commands. When
    /// `Some`, `dispatch` captures the command into the buffer
    /// instead of routing it to the spawn::* handler - used by
    /// respawn tests to assert what would have been
    /// dispatched without spinning up real subprocesses. Always
    /// `None` in production (no enable hook outside test cfg).
    #[cfg(any(test, feature = "testing"))]
    command_intercept: Mutex<Option<Vec<Command>>>,
    /// Test-only project overlay. Entries appended via
    /// `seed_test_project` are searched first in
    /// `find_project_view_by_name` so tests can drive the
    /// Connected-hook respawn trigger without writing a
    /// real `forge.toml`. Empty in production.
    #[cfg(any(test, feature = "testing"))]
    test_extra_projects: Mutex<Vec<LoadedProject>>,
    /// Test-only stand-in for the CLI's per-user preferences document, so
    /// a view fixture reads an ignore preference without the machine's
    /// real `$HOME/.claude.json`. `None` means no fixture seeded one, and
    /// the read goes to the file.
    #[cfg(any(test, feature = "testing"))]
    test_user_preferences: Mutex<Option<serde_json::Value>>,
    /// Test-only stand-in for a seat's live `claude` pid, so a fixture seat's
    /// held loop walks a real process tree with no CLI behind it. A seat with
    /// no entry reads the pid off its own handle, as production does.
    #[cfg(any(test, feature = "testing"))]
    test_claude_pid: Mutex<std::collections::HashMap<SessionSlot, u32>>,
}

/// Pool entry wrapping the live `Arc<AgentHandle>`, the account key
/// the subprocess is bound to, the permission mode that spawn resolved
/// for its project, and the session registration a respawn re-sends.
pub(crate) struct PooledAgent {
    pub handle: Arc<AgentHandle>,
    /// The account the subprocess is bound to, resolved at spawn.
    pub account: AccountKey,
    /// The permission mode this session spawned with, from its
    /// project; `None` when the spawn resolved to no project and the
    /// launcher default applied. The dispatch path reads it to stamp
    /// the same mode onto `/new` and `/resume` re-spawns.
    pub permission_mode: Option<forge_primitives::permission::PermissionMode>,
    /// The session's registration, captured at spawn; a `/new` or
    /// `/resume` respawn re-registers it so the fresh child gets a
    /// fresh binding and env set.
    pub registration: Option<forge_gateway::binding::Registration>,
    /// The id the child currently runs under, as its spawn resolved it
    /// and as `Connected` refreshed it. The slot is the pool's key; this
    /// is the occupant the CLI knows, and what a caller holding only the
    /// slot needs for a CLI argument or a transcript path.
    pub session_id: String,
}

/// Why `insert_live_worker_if_label_absent` refused an insert. Decided
/// under the same lock acquisition as the insert, so the refusal and
/// the pool state can never disagree.
#[derive(Debug)]
pub enum LiveWorkerRefusal {
    /// A live (non-`Failed`) worker already holds the label.
    LabelLive(SessionSlot),
    /// The label's previous despawn is still cleaning up: its worktree is
    /// mid-removal, so a worker admitted onto the label now would be handed
    /// the very directory the removal is about to delete.
    CleanupPending,
    /// The project's worker cap is reached: `live` workers are up
    /// against a cap of `cap`.
    AtCap { live: usize, cap: usize },
}

/// One label's in-flight despawn cleanup, released when it drops.
///
/// Held from the despawn's mark (taken before its teardown) until the
/// cleanup finishes, so a spawn for the same label is refused rather than
/// handed a worktree that the removal is deleting. `Drop` rather than a
/// plain clear at the end: a cleanup that unwinds mid-removal must not
/// leave the label unspawnable for the rest of the run.
pub(crate) struct DespawnCleanupPending {
    workspace: Arc<Workspace>,
    project_key: ProjectKey,
    label: String,
}

impl Drop for DespawnCleanupPending {
    fn drop(&mut self) {
        self.workspace
            .despawn_cleanups
            .lock()
            .remove(&(self.project_key.clone(), self.label.clone()));
    }
}

/// The session a worker label resumes onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeTarget {
    /// A prior session for the label, named by id.
    Found(String),
    /// The label has no prior session to resume.
    None,
}

/// Kick off the catalog scan on the tokio runtime. Idempotent via
/// `started`; a caller with no runtime gets a warn and an
/// immediately-ready flag with an empty catalog rather than a scan
/// nobody can await. A monitor on the scan task publishes readiness
/// even if the scan dies, so a reader never waits on a readiness that
/// will never come.
fn spawn_background_catalog_scan(
    catalog: &Arc<Mutex<HashMap<ProjectKey, Vec<SDKSessionInfo>>>>,
    db: &Arc<Mutex<Option<crate::store::Db>>>,
    config_dir: &Path,
    update_tx: &UpdateFanout,
    loaded: &Arc<std::sync::atomic::AtomicBool>,
    started: &std::sync::atomic::AtomicBool,
) {
    if started.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let run = run_background_catalog_scan(
        Arc::clone(catalog),
        Arc::clone(db),
        config_dir.to_path_buf(),
        update_tx.clone(),
        Arc::clone(loaded),
    );
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let task = handle.spawn(run);
        let loaded = Arc::clone(loaded);
        handle.spawn(async move {
            if let Err(error) = task.await {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "catalog scan task died; publishing readiness so a reader sees an empty catalog rather than waiting",
                );
                loaded.store(true, std::sync::atomic::Ordering::Release);
            }
        });
    } else {
        tracing::warn!(
            target: "forge_workspace::workspace",
            config_dir = %config_dir.display(),
            "no tokio runtime at construction; the catalog scan is skipped and the catalog starts empty",
        );
        loaded.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// One probe call, boxed so the task that runs it is not generic over the
/// future's type.
type CliVersionProbe = std::pin::Pin<Box<dyn std::future::Future<Output = CliVersionInfo> + Send>>;

/// How the probe task gets a snapshot. [`Workspace::new`] passes the real
/// probe; every test constructor passes a script, so the scheduling around
/// the probe is drivable without spawning `claude --version` or reaching
/// npm.
type CliVersionProber = Box<dyn Fn() -> CliVersionProbe + Send + Sync>;

/// The real probe: `claude --version` and npm's `latest` dist-tag.
fn real_cli_version_prober() -> CliVersionProber {
    Box::new(|| Box::pin(forge_agent::env::cli_version::fetch_info()))
}

/// One service-status probe call, boxed for the same reason the version
/// probe's is.
type ServiceStatusProbe =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<ServiceIssue>> + Send>>;

/// How the service-status probe gets its answer. `Workspace::new` passes the
/// real fetch; a test passes a script, so the boot path is drivable without
/// reaching the statuspage.
type ServiceStatusProber = Box<dyn Fn() -> ServiceStatusProbe + Send + Sync>;

/// The real probe: the public statuspage summary.
fn real_service_status_prober() -> ServiceStatusProber {
    Box::new(|| Box::pin(forge_agent::cloud::service_status::fetch_service_status()))
}

/// Kick off the statuspage probe on the tokio runtime: one fetch, then the
/// answer is held. A caller with no runtime gets a warn and holds no status
/// rather than a task nobody would run.
fn spawn_background_service_status_probe(
    started: &std::sync::atomic::AtomicBool,
    service_status: &Arc<Mutex<Option<ServiceIssue>>>,
    update_tx: &UpdateFanout,
    prober: ServiceStatusProber,
) {
    if started.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    // The line the terminal's own module used to write. Without it a probe
    // that stops running is invisible: the only other record is the warning
    // for a missing runtime, which is a different failure.
    tracing::info!(
        target: "forge_workspace::workspace",
        event_name = "service_status_probe_started",
        "the statuspage probe is running",
    );
    let run = run_service_status_probe(Arc::clone(service_status), update_tx.clone(), prober);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(run);
    } else {
        tracing::warn!(
            target: "forge_workspace::workspace",
            event_name = "service_status_probe_skipped",
            "no tokio runtime at construction; the statuspage is not probed and no view reads a service status this run",
        );
    }
}

/// The statuspage probe, off the boot path. Its answer is held before it is
/// announced, so a view that attaches afterwards reads it rather than having
/// missed the one update that carried it.
async fn run_service_status_probe(
    service_status: Arc<Mutex<Option<ServiceIssue>>>,
    update_tx: UpdateFanout,
    prober: ServiceStatusProber,
) {
    let Some(issue) = prober().await else {
        return;
    };
    let update =
        SessionUpdate::ServiceStatus { severity: issue.severity, message: issue.message.clone() };
    *service_status.lock() = Some(issue);
    let _ = update_tx.send(update);
}

/// Kick off the claude version probe on the tokio runtime: one fetch at
/// boot, then a re-probe every [`CLI_VERSION_REFRESH_INTERVAL`]. A caller
/// with no runtime gets a warn and holds no version rather than a task
/// nobody would run.
fn spawn_background_cli_version_probe(
    started: &std::sync::atomic::AtomicBool,
    cli_version: &Arc<Mutex<Option<CliVersionInfo>>>,
    update_tx: &UpdateFanout,
    prober: CliVersionProber,
) {
    if started.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let run = run_cli_version_probe(Arc::clone(cli_version), update_tx.clone(), prober);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(run);
    } else {
        tracing::warn!(
            target: "forge_workspace::workspace",
            event_name = "cli_version_probe_skipped",
            "no tokio runtime at construction; the claude version probe is skipped and the views show no claude version this run",
        );
    }
}

/// The claude version probe, off the boot path. Each result is merged into
/// the store, which wakes the views when it changed what they draw.
async fn run_cli_version_probe(
    cli_version: Arc<Mutex<Option<CliVersionInfo>>>,
    update_tx: UpdateFanout,
    prober: CliVersionProber,
) {
    let mut interval = tokio::time::interval(CLI_VERSION_REFRESH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // The first tick completes immediately, so this is the boot fetch;
        // later ticks only recover a transient miss.
        interval.tick().await;
        let snapshot = prober().await;
        store_cli_version(&cli_version, &update_tx, snapshot);
    }
}

/// Merge one probe result into the store and wake the views when it moved
/// what they draw.
fn store_cli_version(
    store: &Mutex<Option<CliVersionInfo>>,
    update_tx: &UpdateFanout,
    next: CliVersionInfo,
) {
    let changed = {
        let mut held = store.lock();
        let merged = merge_cli_version(held.as_ref(), next);
        // A view draws the two versions, not the snapshot's presence: the
        // first probe that resolves nothing holds a snapshot and changes
        // nothing on screen, so it wakes nobody.
        let held_draws = match held.as_ref() {
            Some(held) => (held.installed.as_deref(), held.latest.as_deref()),
            None => (None, None),
        };
        let merged_draws = (merged.installed.as_deref(), merged.latest.as_deref());
        let changed = held_draws != merged_draws;
        *held = Some(merged);
        changed
    };
    if changed {
        let _ = update_tx.send(SessionUpdate::CliVersionChanged);
    }
}

/// Merge a freshly-probed snapshot over the held one, keeping a
/// previously-resolved field when the new probe came back `None` for it: a
/// failed network probe must not wipe a `latest` an earlier one found.
fn merge_cli_version(prev: Option<&CliVersionInfo>, next: CliVersionInfo) -> CliVersionInfo {
    let Some(prev) = prev else { return next };
    CliVersionInfo {
        installed: next.installed.or_else(|| prev.installed.clone()),
        latest: next.latest.or_else(|| prev.latest.clone()),
    }
}

/// The boot catalog scan, off the boot path. Reads the workspace's own
/// `config_dir` (canonicalized, so the redb tag-cache keys are the same
/// ones any other scan writes even when the config dir path carries a
/// symlink component) with the redb tag cache - only bytes appended
/// since the last scan are read end to end - swaps the grouped catalog
/// in one lock, prunes cache rows of transcripts that no longer exist,
/// and only then publishes readiness and `SessionUpdate::CatalogLoaded`.
async fn run_background_catalog_scan(
    catalog: Arc<Mutex<HashMap<ProjectKey, Vec<SDKSessionInfo>>>>,
    db: Arc<Mutex<Option<crate::store::Db>>>,
    config_dir: PathBuf,
    update_tx: UpdateFanout,
    loaded: Arc<std::sync::atomic::AtomicBool>,
) {
    // The tag cache is one key space with any other scan of this config
    // dir. Without a projects tree the scan still runs and finds an
    // empty catalog.
    let scan_root = catalog_scan_root(&config_dir)
        .unwrap_or_else(|| std::fs::canonicalize(&config_dir).unwrap_or(config_dir.clone()));
    let tag_cache = std::sync::Arc::new(load_session_tag_cache(db.lock().as_ref()));
    let catalog_entries = forge_agent::userdata::catalog::scan::list_sessions(
        &scan_root,
        None, // every project in the catalog
        None, // no limit
        0,
        forge_agent::userdata::catalog::scan::Workers::Hidden,
        Some(&tag_cache),
    )
    .await;
    persist_session_tag_cache(db.lock().as_ref(), &tag_cache);

    // Group sessions by project key derived from each session's cwd.
    // Sessions without a cwd are skipped - they can't be associated
    // with a project view.
    let mut grouped: HashMap<ProjectKey, Vec<SDKSessionInfo>> = HashMap::new();
    for entry in catalog_entries {
        if let Some(cwd) = entry.cwd.as_deref() {
            let key = ProjectKey::new(
                forge_agent::userdata::catalog::scan::project_key_for_directory(Some(cwd)),
            );
            grouped.entry(key).or_default().push(entry);
        }
    }
    // The catalog scan returns entries sorted by `last_modified`
    // descending; the per-project Vec inherits that ordering thanks
    // to push order being preserved.
    //
    // Sessions recorded live while the scan ran sit in the catalog
    // already (record_connected_session) and their transcripts may not
    // be on disk yet, so the disk-built map absorbs them rather than
    // replacing them.
    {
        let mut current = catalog.lock();
        for (key, entries) in current.drain() {
            let slot = grouped.entry(key).or_default();
            // The recorded list is newest-first; inserting each at the
            // head in reverse lands the newest back on top.
            for entry in entries.into_iter().rev() {
                if !slot.iter().any(|s| s.session_id == entry.session_id) {
                    slot.insert(0, entry);
                }
            }
        }
        *current = grouped;
    }

    if let Some(db) = db.lock().as_ref() {
        match crate::store::session_tags::prune_missing(db) {
            Ok(0) => {}
            Ok(count) => tracing::info!(
                target: "forge_workspace::workspace",
                count,
                "pruned session tag rows whose transcripts no longer exist",
            ),
            Err(error) => tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "pruning stale session tag rows failed; the table keeps rows of deleted transcripts",
            ),
        }
    }

    loaded.store(true, std::sync::atomic::Ordering::Release);
    let _ = update_tx.send(SessionUpdate::CatalogLoaded);
}

/// The previous run's tag scans, or an empty cache when there is no
/// store: a missing cache costs a full re-scan, never a wrong answer.
fn load_session_tag_cache(
    db: Option<&crate::store::Db>,
) -> forge_agent::userdata::catalog::scan::SessionTagCache {
    let prior = db
        .map(|db| {
            crate::store::session_tags::load_all(db).unwrap_or_else(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "loading the session tag cache failed; every transcript will be re-scanned",
                );
                std::collections::HashMap::new()
            })
        })
        .unwrap_or_default();
    forge_agent::userdata::catalog::scan::SessionTagCache::new(prior)
}

fn persist_session_tag_cache(
    db: Option<&crate::store::Db>,
    cache: &forge_agent::userdata::catalog::scan::SessionTagCache,
) {
    let updates = cache.updates();
    if let Some(db) = db
        && let Err(error) = crate::store::session_tags::store_all(db, &updates)
    {
        tracing::warn!(
            target: "forge_workspace::workspace",
            %error,
            "storing the session tag cache failed; the next boot re-scans what it covered",
        );
    }
}

/// Open the machine-local redb store at `<app_support>/db.redb`,
/// creating the app-support dir first. Returns `None` (with a warn) when
/// the dir can't be created or the DB can't open - forge then runs
/// without durable crons, subscriptions, tasks or dynamic workers this
/// session (hard rule #14: no cwd fallback).
fn open_db(app_support: &Path) -> Option<crate::store::Db> {
    if let Err(error) = std::fs::create_dir_all(app_support) {
        tracing::warn!(
            target: "forge_workspace::workspace",
            %error,
            path = %app_support.display(),
            "creating the app-support dir failed; durable crons, subscriptions, tasks and dynamic workers will not persist",
        );
        return None;
    }
    match crate::store::Db::open(&app_support.join("db.redb")) {
        Ok(db) => Some(db),
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "opening the redb store failed; durable crons, subscriptions, tasks and dynamic workers will not persist",
            );
            None
        }
    }
}

/// The configured projects as the session store keys them: the catalog
/// key a `dynamic_workers` row carries, plus the org and name a
/// `sessions` row is keyed by.
fn project_identities(config: &LoadedConfig) -> Vec<crate::store::sessions::ProjectIdentity> {
    config
        .projects
        .iter()
        .map(|project| crate::store::sessions::ProjectIdentity {
            key: forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                &project.path.to_string_lossy(),
            )),
            org: project.org.clone(),
            name: project.name.clone(),
        })
        .collect()
}

/// The scan root for one config dir: the canonical parent of its
/// resolved `projects` dir, or the canonical config dir itself when
/// the resolved tree is not named `projects` (the walk descends into
/// `<root>/projects`, so the target's parent would miss it). `None`
/// when there is no projects tree.
fn catalog_scan_root(config_dir: &std::path::Path) -> Option<PathBuf> {
    let projects = match std::fs::canonicalize(config_dir.join("projects")) {
        Ok(projects) => projects,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                target: "forge_workspace::workspace",
                config_dir = %config_dir.display(),
                "no projects tree; the catalog scan finds nothing here",
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::workspace",
                config_dir = %config_dir.display(),
                %error,
                "cannot resolve this config dir's projects tree; its sessions are invisible to the scan",
            );
            return None;
        }
    };
    if projects.file_name().is_some_and(|name| name.to_str() == Some("projects")) {
        Some(projects.parent().map_or_else(|| config_dir.to_path_buf(), PathBuf::from))
    } else {
        Some(std::fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf()))
    }
}

/// A forge command's answer: what the reader is told, and how loudly.
///
/// **Returned rather than emitted where the command is handled**, because the
/// answer has to land AFTER the words that asked for it: the dispatcher echoes
/// those once the command has landed, and a notice sent from the handler
/// arrived first, which every client that draws the stream in order put above
/// the prompt it answers.
type ForgeAnswer = (NoticeSeverity, String);

/// The line a forge name invoked wrongly is answered with.
///
/// Answered rather than acted on, and never left to fall through as a prompt:
/// a mistyped command reaching the model reads as a question.
fn forge_misuse(usage: &str) -> ForgeAnswer {
    (NoticeSeverity::Error, usage.to_owned())
}

impl Workspace {
    /// Builds a Workspace, kicks off the background catalog scan and the
    /// claude version probe, and loads `<config_dir>/forge.toml`. Errors if
    /// `forge.toml` is missing or malformed (e.g. no `[[orgs]]` entries, no
    /// `[[orgs.projects]]` entries, unknown account references). No
    /// Agents are spawned on success. The session catalog starts empty
    /// and fills when the scan lands - see [`Workspace::start_catalog_scan`].
    pub fn new(config_dir: PathBuf) -> Result<Self, WorkspaceError> {
        Self::new_impl(
            config_dir,
            None,
            true,
            Some(real_cli_version_prober()),
            Some(real_service_status_prober()),
        )
    }

    /// Like [`Workspace::new`] but puts forge's whole app-support base -
    /// the redb store and the single-instance lock - under a tempdir
    /// inside `config_dir` rather than the real machine
    /// `app_support_dir`, so tests never touch the user's durable store
    /// or contend for their live lock. The catalog scan does NOT
    /// auto-start; tests opt in via [`Workspace::start_catalog_scan`]
    /// so the catalog stays empty until a fixture asks for it. Neither
    /// boot probe starts either: a test workspace reaches no network.
    #[cfg(any(test, feature = "testing"))]
    pub fn new_for_test(config_dir: PathBuf) -> Result<Self, WorkspaceError> {
        Self::new_for_test_impl(config_dir, None)
    }

    /// [`Workspace::new_for_test`] with `prober` driving the claude version
    /// probe in place of the real one, so a test drives the constructor's
    /// own launch without spawning `claude --version` or reaching npm.
    #[cfg(any(test, feature = "testing"))]
    pub fn new_for_test_with_cli_version_prober(
        config_dir: PathBuf,
        prober: CliVersionProber,
    ) -> Result<Self, WorkspaceError> {
        Self::new_for_test_impl(config_dir, Some(prober))
    }

    #[cfg(any(test, feature = "testing"))]
    fn new_for_test_impl(
        config_dir: PathBuf,
        cli_version_prober: Option<CliVersionProber>,
    ) -> Result<Self, WorkspaceError> {
        let app_support = config_dir.join("app-support");
        let workspace =
            Self::new_impl(config_dir, Some(app_support), false, cli_version_prober, None)?;
        // Tests never start the listener; the boot gate reads open so
        // spawn paths are exercisable, and the I2 test flips it back to
        // closed explicitly when it needs the refusal.
        workspace.gateway_ready.store(true, std::sync::atomic::Ordering::Release);
        Ok(workspace)
    }

    /// Run the catalog scan in the background and signal readiness
    /// when it lands. `[`Workspace::new`] calls this during
    /// construction; tests built on `new_for_test` opt in explicitly so
    /// the catalog stays empty until a fixture asks for it. Idempotent -
    /// a second call is a no-op.
    pub fn start_catalog_scan(&self) {
        spawn_background_catalog_scan(
            &self.catalog,
            &self.db,
            &self.config_dir,
            &self.update_tx,
            &self.catalog_loaded,
            &self.catalog_scan_started,
        );
    }

    /// Whether the background catalog scan has populated the catalog.
    /// False from construction until the scan lands; always true when
    /// constructed without a tokio runtime (nothing would run it).
    ///
    /// Test-only: the boot spawn hold that read this is gone, and a
    /// frontend learns the same fact from `SessionUpdate::CatalogLoaded`.
    /// Tests await it to sequence a fixture against the scan.
    #[cfg(any(test, feature = "testing"))]
    pub fn catalog_ready(&self) -> bool {
        self.catalog_loaded.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Kick off the claude version probe on the tokio runtime: one fetch at
    /// boot, then a re-probe every [`CLI_VERSION_REFRESH_INTERVAL`]. The
    /// constructor is the only production caller, and the prober it takes
    /// is what lets a test drive this same launch. Idempotent - a second
    /// call is a no-op.
    fn start_cli_version_probe(&self, prober: CliVersionProber) {
        spawn_background_cli_version_probe(
            &self.cli_version_probe_started,
            &self.cli_version,
            &self.update_tx,
            prober,
        );
    }

    /// The `claude` CLI versions the core holds: what is installed on this
    /// machine and what npm publishes, both `None` until the boot probe
    /// resolves them. The same answer for every viewer, so a view reads it
    /// here rather than probing for itself. A change arrives as
    /// [`SessionUpdate::CliVersionChanged`].
    pub fn cli_version(&self) -> Option<CliVersionInfo> {
        self.cli_version.lock().clone()
    }

    /// Shared constructor body. `app_support` supplies the app-support
    /// base dir; `None` resolves the real machine `app_support_dir` and
    /// degrades to no lock and no durable store on failure (hard rule
    /// #14: no cwd fallback).
    ///
    /// `cli_version_prober` and `service_status_prober` are `None` for a
    /// workspace built by a test. Both probes fetch over the network at boot
    /// and announce the answer on the core's own stream, so a test workspace
    /// that ran them would both depend on a remote service's health and hand
    /// every test a slot-less update it never emitted.
    fn new_impl(
        config_dir: PathBuf,
        app_support: Option<PathBuf>,
        catalog_scan: bool,
        cli_version_prober: Option<CliVersionProber>,
        service_status_prober: Option<ServiceStatusProber>,
    ) -> Result<Self, WorkspaceError> {
        let config = load_from_dir(&config_dir)?;

        // Create forge's own config subfolder before anything writes into
        // it (the lock, the cron + state stores all live under it). Hard-
        // fail if it can't be created: forge can persist nothing without a
        // writable config dir, so degrading is pointless.
        crate::config::ensure_forge_data_dir(&config_dir).map_err(|source| {
            WorkspaceError::DataDirUnavailable {
                path: crate::config::forge_data_dir(&config_dir),
                source,
            }
        })?;

        // forge's machine-local app-support base, resolved once: the
        // single-instance lock and the redb store both sit under it.
        // `Some` comes from the test constructor, which points the whole
        // base at a tempdir. `None` degrades to no lock and no durable
        // store rather than falling back to cwd (hard rule #14).
        let app_support = match app_support {
            Some(dir) => Some(dir),
            None => match forge_sdk::app_support_dir() {
                Ok(dir) => Some(dir),
                Err(error) => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        %error,
                        "app-support dir unresolved; the single-instance guard is skipped and durable state will not persist",
                    );
                    None
                }
            },
        };

        // Single-instance guard: forge runs one process per config dir.
        // The held File is stored on `Self` for the process lifetime;
        // flock auto-releases on exit/crash. A second forge on the same
        // config dir hard-fails here.
        let single_instance_lock = match app_support.as_deref() {
            Some(base) => match crate::single_instance::acquire(&config_dir, base) {
                Ok(lock) => lock,
                Err(crate::single_instance::AcquireError::AlreadyRunning { pid }) => {
                    return Err(WorkspaceError::AlreadyRunning { pid });
                }
            },
            None => None,
        };
        // The guard fell open (lockfile unopenable or flock unsupported).
        // forge still boots, but the cron store's single-writer assumption
        // no longer holds - surface it loudly rather than leaving it at the
        // module-internal warn `acquire` already logged.
        if single_instance_lock.is_none() {
            tracing::error!(
                target: "forge_workspace::workspace",
                config_dir = %config_dir.display(),
                "single-instance guard unavailable; on-disk state (crons, usage cache, settings) is NOT protected against a second forge on this config dir - check the dir is writable and on a flock-capable filesystem",
            );
        }

        // Open the machine-local redb store: durable crons, Gotify and Slack
        // subscriptions all live in it. A failure to resolve the app-
        // support dir or open the DB degrades to no durable state this run
        // (non-fatal, like the state cache) - no cwd fallback (hard rule #14).
        let mut db = app_support.as_deref().and_then(open_db);

        // Load durable forge crons into the in-memory working set. Boot
        // catch-up for entries that came due while forge was down runs after
        // construction, once the dispatch machinery is live.
        let crons = match &db {
            Some(db) => crate::store::cron::list(db).unwrap_or_else(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "loading durable crons failed; starting with none this run",
                );
                Vec::new()
            }),
            None => Vec::new(),
        };
        let tasks = match &db {
            Some(db) => crate::store::tasks::list(db).unwrap_or_else(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "loading durable tasks failed; starting with none this run",
                );
                Vec::new()
            }),
            None => Vec::new(),
        };
        let gotify_subs = match &db {
            Some(db) => crate::store::gotify::list(db).unwrap_or_else(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "loading durable Gotify subscriptions failed; starting with none",
                );
                Vec::new()
            }),
            None => Vec::new(),
        };
        let mut slack_load_failed = false;
        let slack_subs = match &db {
            Some(db) => crate::store::slack::list(db).unwrap_or_else(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "loading durable Slack subscriptions failed; starting with none",
                );
                slack_load_failed = true;
                Vec::new()
            }),
            None => Vec::new(),
        };

        // The boot moves anything still in `dynamic_workers` onto
        // `sessions`, so a worker persisted before this build has a row
        // the re-spawn wave can read, and drops the retired table once
        // nothing is left in it. Non-fatal, like the loads above.
        let mut swept_clean = false;
        if let Some(db) = &db {
            match crate::store::sessions::migrate_from_dynamic_workers(
                db,
                &project_identities(&config),
            ) {
                Ok(outcome) => {
                    if outcome.unkeyable > 0 {
                        tracing::warn!(
                            target: "forge_workspace::workspace",
                            unkeyable = outcome.unkeyable,
                            "the retired worker table holds rows no configured project can key; \
                             they stay there and the table is not dropped",
                        );
                    }
                    swept_clean = outcome.drained();
                }
                Err(error) => tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "migrating persisted workers into the sessions table failed; the retired \
                     table is left in place for the next boot",
                ),
            }
        }
        // Drop the retired table only once every row it held has been
        // copied, and reclaim its pages - a copied table is still
        // allocated. A sweep that errored, that left a row it could not
        // key, or that did not run at all leaves rows there and only
        // there, so dropping on any of those would lose a worker the user
        // believes is durable, silently and with nothing to say which
        // label went. A store that never had the table drops nothing.
        if swept_clean && let Some(db) = db.as_mut() {
            match crate::store::dynamic_workers::drop_table(db) {
                Ok(true) => {
                    if let Err(error) = db.compact() {
                        tracing::warn!(
                            target: "forge_workspace::workspace",
                            %error,
                            "compacting the store after dropping the retired worker table failed",
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "dropping the retired dynamic_workers table failed; it stays for the next boot",
                ),
            }
        }
        // The catalog table outlived its module: deleting the machinery
        // left the rows behind, and nothing reads or writes them now, so
        // there is nothing to drain first. A store that never cached a
        // catalog drops nothing.
        if let Some(db) = db.as_mut() {
            match crate::store::model_catalog::drop_table(db) {
                Ok(true) => {
                    if let Err(error) = db.compact() {
                        tracing::warn!(
                            target: "forge_workspace::workspace",
                            %error,
                            "compacting the store after dropping the retired model catalog table failed",
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "dropping the retired model_catalog table failed; it stays for the next boot",
                ),
            }
        }
        // The settings table held one row, the `/spinner` override, which went
        // with the spinner.
        if let Some(db) = db.as_mut() {
            match crate::store::settings::drop_table(db) {
                Ok(true) => {
                    if let Err(error) = db.compact() {
                        tracing::warn!(
                            target: "forge_workspace::workspace",
                            %error,
                            "compacting the store after dropping the retired settings table failed",
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "dropping the retired settings table failed; it stays for the next boot",
                ),
            }
        }

        // Resolved here rather than lazily so a malformed `[[slack]]`
        // entry refuses the boot, the way the rest of forge.toml does.
        // The client goes through the same TLS-trust helper the Gotify
        // seam uses, so NODE_EXTRA_CA_CERTS behaves identically.
        let slack_http = forge_agent::http_trust::with_extra_roots(reqwest::Client::builder())
            .build()
            .map_err(|error| WorkspaceError::ConfigInvalid {
                path: crate::config::forge_data_dir(&config_dir).join("forge.toml"),
                message: format!("slack http client: {error}"),
            })?;
        let slack = Arc::new(
            crate::slack::SlackWorkspaces::from_config(&config.slack, &slack_http).map_err(
                |message| WorkspaceError::ConfigInvalid {
                    path: crate::config::forge_data_dir(&config_dir).join("forge.toml"),
                    message,
                },
            )?,
        );
        let systemone = crate::systemone::client_for(
            config.systemone.as_ref(),
            &crate::config::forge_data_dir(&config_dir).join("forge.toml"),
        )?
        .map(Arc::new);

        // Catalog scan reads against the workspace's canonical
        // `config_dir` (where forge.toml lives). Each spawn binds to
        // its own account `config_dir` separately; multi-account
        // catalog merge is a separate concern.
        // The scan itself runs in the background (#794): reading every
        // transcript end to end for its worker tag was the bulk of the
        // pre-paint pause. The catalog starts empty and fills when the
        // scan lands; nothing on a spawn path waits for it.
        let catalog = Arc::new(Mutex::new(HashMap::new()));

        let accounts = Arc::new(forge_gateway::AccountPool::new(&config.accounts));
        // The forward leg's client comes from the host port, like every
        // other outbound path: the inference upstream then carries the
        // same extra trust roots the probes do.
        let forward_http = forge_agent::cloud::AgentHost
            .streaming_http_client(
                forge_gateway::forward::FORWARD_CONNECT_TIMEOUT,
                forge_gateway::forward::FORWARD_IDLE_TIMEOUT,
            )
            .map_err(|error| WorkspaceError::ConfigInvalid {
                path: crate::config::forge_data_dir(&config_dir).join("forge.toml"),
                message: format!("forward-leg http client: {error}"),
            })?;
        let gateway =
            Arc::new(forge_gateway::forward::Gateway::new(Arc::clone(&accounts), forward_http));
        // Hand the gateway each org's walk order so an unbound session
        // can select an account from its path alone - the restart case,
        // where the bindings are gone but the config is boot-frozen.
        {
            let mut pins: HashMap<String, forge_gateway::selection::OrgPin> = HashMap::new();
            for project in &config.projects {
                let pin = pins.entry(project.org.clone()).or_insert_with(|| {
                    forge_gateway::selection::OrgPin {
                        accounts: project.accounts.clone(),
                        fallback_accounts: project.fallback_accounts.clone(),
                    }
                });
                // Orgs are load-validated to share one pin; differing
                // copies would mean a config bug, so the first wins and
                // the rest are ignored the way duplicates elsewhere are.
                debug_assert_eq!(pin.accounts, project.accounts, "org pin drifted");
                debug_assert_eq!(
                    pin.fallback_accounts, project.fallback_accounts,
                    "org pin drifted"
                );
            }
            gateway.set_org_pins(pins);
        }
        gateway.set_rotation_numbers(config.gateway_rotation);

        // Seed account usage from the machine-local store so the
        // launchpad picker has usage data immediately at cold boot.
        // Anthropic's /api/oauth/usage rate-limiter can stall the first
        // live probe for 30 s+; without seed data every account reads as
        // unknown during that window, so nothing is demoted for being at
        // its cap. The 60 s background poller refreshes these snapshots -
        // the cache is purely "last known value" seed.
        let state = match &db {
            Some(db) => crate::account_cache::load(db),
            None => crate::account_cache::ForgeState::empty(),
        };
        accounts.seed_from_cache(&state.account_usage);

        let gateway_port = config.gateway_port;
        let update_tx = UpdateFanout::default();
        let (kick_dispatcher_tx, kick_dispatcher_rx) = mpsc::unbounded_channel::<KickRequest>();
        let config_dictate = config.dictate.clone();
        let db = Arc::new(Mutex::new(db));
        let catalog_loaded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let catalog_scan_started = std::sync::atomic::AtomicBool::new(false);
        if catalog_scan {
            spawn_background_catalog_scan(
                &catalog,
                &db,
                &config_dir,
                &update_tx,
                &catalog_loaded,
                &catalog_scan_started,
            );
        }
        // The version facts are the same for every viewer, so the core
        // probes them once and both views read one answer.
        let cli_version = Arc::new(Mutex::new(None));
        // The statuspage answer is the same for every viewer too, and it
        // has to outlive the view that used to fetch it.
        let service_status: Arc<Mutex<Option<ServiceIssue>>> = Arc::new(Mutex::new(None));
        let workspace = Self {
            config_dir,
            config,
            systemone,
            catalog,
            pool: Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "testing"))]
            test_spawn_handle: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_spawn_listing: Mutex::new(RecordedListing::None),
            #[cfg(any(test, feature = "testing"))]
            test_spawn_server: Mutex::new(None),
            accounts,
            gateway,
            gateway_ready: std::sync::atomic::AtomicBool::new(false),
            gateway_port,
            gateway_url: Mutex::new(None),
            gateway_bind_error: Mutex::new(None),
            dictate: Arc::new(crate::dictate::DictateState::new(&config_dictate)),
            dictate_runtime: Mutex::new(crate::dictate::DictateRuntime::default()),
            dictate_device_pick: Mutex::new(None),
            browser: Arc::new(crate::browser::BrowserRelay::new()),
            dictate_catalogue: Mutex::new(crate::catalogue::CatalogueState::default()),
            dictate_install: Mutex::new(crate::install::InstallState::default()),
            dictate_activate: Mutex::new(crate::install::ActivateState::default()),
            dictate_bench: Mutex::new(crate::bench::BenchState::default()),
            dictate_bench_cancel: std::sync::atomic::AtomicBool::new(false),
            read_aloud_error: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_catalogue_source: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_cleanup_source: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_catalogue_dir: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_read_aloud_dir: Mutex::new(None),
            update_tx,
            command_senders: Mutex::new(HashMap::new()),
            live_workers: Mutex::new(HashMap::new()),
            despawn_cleanups: Mutex::new(HashSet::new()),
            spawn_failures: Mutex::new(HashMap::new()),
            held_work_seats: crate::work::HeldSeats::default(),
            domain_handles: Mutex::new(HashMap::new()),
            review_origin: Mutex::new(HashMap::new()),
            review_activity: Mutex::new(HashMap::new()),
            usage_poller_started: std::sync::atomic::AtomicBool::new(false),
            cron_scheduler_started: std::sync::atomic::AtomicBool::new(false),
            chase_sweep_started: std::sync::atomic::AtomicBool::new(false),
            auto_continue_sweep_started: std::sync::atomic::AtomicBool::new(false),
            kick_dispatcher_tx,
            kick_dispatcher_rx_slot: Mutex::new(Some(kick_dispatcher_rx)),
            _single_instance_lock: single_instance_lock,
            crons: Mutex::new(crons),
            tasks: Mutex::new(tasks),
            parked_by_slot: Mutex::new(HashMap::new()),
            gotify_subs: Mutex::new(gotify_subs),
            db,
            catalog_loaded,
            catalog_scan_started,
            cli_version,
            last_fatal_error: Mutex::new(None),
            service_status,
            service_status_probe_started: std::sync::atomic::AtomicBool::new(false),
            cli_version_probe_started: std::sync::atomic::AtomicBool::new(false),
            gotify_connected: Mutex::new(false),
            gotify_app_index: Mutex::new(HashMap::new()),
            gotify_subsystem: Mutex::new(None),
            slack,
            slack_subs: Mutex::new(slack_subs),
            slack_subsystem: Mutex::new(std::collections::BTreeMap::new()),
            slack_connected: Mutex::new(std::collections::BTreeMap::new()),
            slack_user_ids: Mutex::new(std::collections::BTreeMap::new()),
            slack_user_names: Mutex::new(std::collections::BTreeMap::new()),
            slack_author_failures: Mutex::new(std::collections::HashSet::new()),
            slack_drafts: Mutex::new(HashMap::new()),
            browser_handoffs: Mutex::new(HashMap::new()),
            slack_recently_delivered: Mutex::new(HashMap::new()),
            slack_load_failed: std::sync::atomic::AtomicBool::new(slack_load_failed),
            slack_user_id_retries: Mutex::new(std::collections::BTreeMap::new()),
            slack_verification_started: std::sync::atomic::AtomicBool::new(false),
            respawn_in_flight: Mutex::new(std::collections::HashSet::new()),
            unwakeable_crons: Mutex::new(std::collections::HashSet::new()),
            #[cfg(any(test, feature = "testing"))]
            command_intercept: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_extra_projects: Mutex::new(Vec::new()),
            #[cfg(any(test, feature = "testing"))]
            test_user_preferences: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_claude_pid: Mutex::new(std::collections::HashMap::new()),
        };
        if let Some(prober) = cli_version_prober {
            workspace.start_cli_version_probe(prober);
        }
        if let Some(prober) = service_status_prober {
            workspace.start_service_status_probe(prober);
        }
        if workspace.db.lock().is_none() {
            // One user-visible notice for the whole best-effort-persist
            // class (durable crons, subscriptions): the
            // store is gone this run, so every one of those warns would
            // otherwise fire per-op into the log only.
            let _ = workspace.update_tx.send(SessionUpdate::ServiceStatus {
                severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                message: "Machine-local store unavailable this run; crons, tasks, Gotify and Slack subscriptions will not persist".to_owned(),
            });
        }
        Ok(workspace)
    }

    /// Effective `[server]` settings: whether the socket's listener starts
    /// and where it binds. Read by the binary entry point, which serves the
    /// socket - the workspace never does.
    pub fn server_config(&self) -> forge_primitives::ServerConfig {
        self.config.server.clone()
    }

    /// Effective `[client]` settings: which of the sets forge ships a
    /// client draws with. Carried by the greeting, so a client never reads
    /// `forge.toml` itself.
    pub fn client_config(&self) -> forge_primitives::ClientConfig {
        self.config.client.clone()
    }

    /// The browser relay: the one client connection that drives the browser.
    /// Read by the binary entry point, which hands it to the transport so a
    /// capable connection can take the role, and by the browser family's
    /// facade, which sends asks through it.
    pub fn browser_relay(&self) -> Arc<crate::browser::BrowserRelay> {
        Arc::clone(&self.browser)
    }

    /// The push-to-talk key from forge.toml `[dictate] bind`. Read by
    /// the TUI's key handler per event; config is boot-frozen so the
    /// value never changes mid-run.
    pub fn dictate_bind(&self) -> crate::dictate::DictateBind {
        self.config.dictate.bind
    }

    /// How press/release maps onto starting and stopping a take, from
    /// forge.toml `[dictate] mode`. Read by the TUI's key handler per
    /// event; config is boot-frozen so the value never changes mid-run.
    pub fn dictate_mode(&self) -> crate::dictate::DictateMode {
        self.config.dictate.mode
    }

    /// Whether dictation is on at all. The key handler reads this so a
    /// press with `[dictate]` disabled is dead rather than a refusal:
    /// the box doc's S0 is "nothing at all", and with the section
    /// absent every Cmd chord would otherwise pop an error.
    pub fn dictate_enabled(&self) -> bool {
        self.config.dictate.enabled
    }

    /// The `[plugins]` auto-update policy from forge.toml. Read by the
    /// plugins pane for the boot auto-update gate and the pane's
    /// auto-update state display.
    pub fn plugin_settings(&self) -> &crate::config::PluginSettings {
        &self.config.plugins
    }

    /// Remember plugin updates applied by a pane run or by boot
    /// auto-update: the visible record of what moved, from where, and
    /// the ref a rollback restores. One transaction for the whole
    /// batch. A no-op with a warn when the store is closed.
    pub fn record_plugin_updates(&self, records: &[forge_primitives::plugins::PluginUpdateRecord]) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::plugins::record_updates(db, records)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                count = records.len(),
                error = %error,
                "failed to persist the plugin update records",
            );
        }
    }

    /// Every remembered plugin update, latest write per installed
    /// entry. Empty when the store is closed or unreadable.
    pub fn plugin_update_records(&self) -> Vec<forge_primitives::plugins::PluginUpdateRecord> {
        self.db
            .lock()
            .as_ref()
            .and_then(|db| crate::store::plugins::update_records(db).ok())
            .map(|records| records.into_values().collect())
            .unwrap_or_default()
    }

    /// Forget the record for one installed entry after its rollback
    /// ran: the previous version it named is now current.
    pub fn clear_plugin_update_record(&self, plugin_id: &str, scope: &str) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::plugins::clear_update_record(db, plugin_id, scope)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                plugin = %plugin_id,
                error = %error,
                "failed to clear a plugin update record",
            );
        }
    }

    /// Return the names of all projects that should spawn at forge
    /// launch (`auto_start = true`). Order is declaration order from
    /// forge.toml - the launchpad picker uses its own row sort, so
    /// no further ordering is imposed here.
    pub fn auto_start_project_names(&self) -> Vec<String> {
        self.config.auto_start_projects().map(|p| p.name.clone()).collect()
    }

    /// Every project listed in `forge.toml`, each carrying its catalog
    /// sessions sorted by last-activity descending - `sessions[0]` is
    /// the lead. Empty `sessions` means the project has nothing on
    /// disk yet; the project still surfaces in the returned Vec.
    pub fn list_projects(&self) -> Vec<ProjectView> {
        // A catalog row is a transcript file, named by the id the CLI
        // wrote it under, so openness is answered by the ids forge
        // currently holds rather than by the row's slot.
        let open_sessions: std::collections::HashSet<String> =
            self.pool.lock().values().map(|entry| entry.session_id.clone()).collect();

        // One catalog acquire for the whole walk - the loop body is
        // a HashMap lookup + bounded cloning over owned session info
        // and never re-enters the catalog, so holding the lock for
        // the duration is cheap. The prior per-iteration
        // acquire/drop showed up as the dominant hot spot in
        // `ui::render` under projects with ~14 entries.
        let catalog = self.catalog.lock();
        // Iterate config.projects, with the test-only overlay appended
        // so unit tests can seed projects via `seed_test_project`
        // and exercise paths that read `list_projects` (matches the
        // `project_root_for_key` / `find_project_view_by_name` overlay
        // pattern).
        #[cfg(any(test, feature = "testing"))]
        let extra = self.test_extra_projects.lock();
        let project_iter: Box<dyn Iterator<Item = &LoadedProject>> = {
            #[cfg(any(test, feature = "testing"))]
            {
                Box::new(self.config.projects.iter().chain(extra.iter()))
            }
            #[cfg(not(any(test, feature = "testing")))]
            {
                Box::new(self.config.projects.iter())
            }
        };
        let mut views = Vec::with_capacity(self.config.projects.len());
        for project in project_iter {
            let key =
                ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
                    Some(&project.path.to_string_lossy()),
                ));

            let sessions: Vec<SessionView> = catalog
                .get(&key)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|info| SessionView {
                            session: forge_primitives::SessionId::new(info.session_id.clone()),
                            label: info.summary.clone(),
                            is_open: open_sessions.contains(&info.session_id),
                            last_activity: Some(
                                UNIX_EPOCH + Duration::from_millis(info.last_modified),
                            ),
                        })
                        .collect()
                })
                .unwrap_or_default();

            views.push(ProjectView {
                key,
                name: project.name.clone(),
                org: project.org.clone(),
                path: project.path.clone(),
                display_path: project.display_path.clone(),
                accounts: project.accounts.clone(),
                fallback_accounts: project.fallback_accounts.clone(),
                has_model: project.model.is_some(),
                sessions,
            });
        }
        views
    }

    /// Validate that `name` matches a project in `forge.toml`. Used
    /// by callers (e.g. forge-tui's main) to fail fast on an unknown
    /// CLI positional arg before TUI setup, rather than surface the
    /// same error later via [`Self::get_agent_handle`].
    pub fn validate_project_name(&self, name: &str) -> Result<(), WorkspaceError> {
        self.find_project_by_name(name).map(|_| ())
    }

    /// Stamp [`LEAD_DELEGATION_PREAMBLE`] onto a Lead session's launch
    /// settings. No-op for a worker.
    fn apply_lead_delegation(settings: &mut SessionLaunchSettings, kind: crate::mcp::SessionKind) {
        if matches!(kind, crate::mcp::SessionKind::Lead) {
            settings.delegation_preamble = Some(LEAD_DELEGATION_PREAMBLE.to_owned());
        }
    }

    /// The slot a spawn fills, from the role its caller stated. The label
    /// is used as given: a worker must never be resolved through the
    /// registry, which answers with the lead's slot once its entry is
    /// gone, and nothing recovers either answer from the key's shape - a
    /// key that looks like a worker is not evidence, and a project named
    /// `worker_foo` once classified as a worker for exactly that reason.
    /// The slot a spawn's role states under `project`. The one source
    /// for both a session's tool surface and its address, so the two
    /// cannot disagree.
    fn slot_for_spawn(
        role: &crate::protocol::SpawnRole,
        project: &LoadedProject,
    ) -> crate::SessionSlot {
        match role {
            crate::protocol::SpawnRole::Lead => {
                crate::SessionSlot::lead(&project.org, &project.name)
            }
            crate::protocol::SpawnRole::Worker { label, .. } => {
                crate::SessionSlot::worker(&project.org, &project.name, label)
            }
        }
    }

    /// Hands out the `Arc<AgentHandle>` for the requested session,
    /// spawning the underlying Agent lazily if it isn't already pooled.
    /// Idempotent - repeated calls for the same target return the same
    /// handle (no second subprocess). `settings` only apply to the
    /// spawn; subsequent calls reuse the existing Agent and ignore the
    /// parameter.
    ///
    /// Each fresh spawn runs the gateway's account selection and
    /// stamps the chosen account's env onto the spawned `claude`
    /// subprocess, so it talks to the gateway listener rather than
    /// upstream. Selection state lives in the in-memory usage cache;
    /// nothing about account choice is persisted across forge
    /// launches.
    ///
    /// Workspace does not track which handle the caller is "using" -
    /// that's the caller's concern.
    /// `role` is what the caller states this session to be. There is no
    /// keyless form: a role forge cannot state is one it would have to
    /// guess from the live-worker registry, which answers with the lead's
    /// slot for a worker whose entry is gone.
    pub fn get_agent_handle(
        self: &Arc<Self>,
        target: SessionTarget,
        settings: SessionLaunchSettings,
        role: &crate::protocol::SpawnRole,
    ) -> Result<Arc<AgentHandle>> {
        self.get_agent_handle_at_key(target, settings, None, role)
    }

    /// Like [`Self::get_agent_handle`] but takes the key the caller
    /// already resolved, so the pool entry lands under the same key the
    /// caller announced. `None` for callers that have not resolved one.
    ///
    /// `role` is what the caller states this spawn to be. It is the one
    /// source for both the tool surface and the slot's label, so a worker
    /// cannot be handed a lead's tool surface or a lead's address.
    pub(crate) fn get_agent_handle_at_key(
        self: &Arc<Self>,
        target: SessionTarget,
        mut settings: SessionLaunchSettings,
        resolved_key: Option<SessionSlot>,
        role: &crate::protocol::SpawnRole,
    ) -> Result<Arc<AgentHandle>> {
        // Every session forge launches passes through here, so the tools
        // forge has replaced are denied at this one point rather than at
        // each spawn entry - a fourth entry cannot forget them. The role
        // states the kind, and the slot it is checked against below cannot
        // disagree with it.
        let spawn_kind = match role {
            crate::protocol::SpawnRole::Lead => crate::mcp::SessionKind::Lead,
            crate::protocol::SpawnRole::Worker { .. } => crate::mcp::SessionKind::Worker,
        };
        // `interactive` is passed as `true` because this point cannot know
        // it, and the question denial must not be decided wrongly here: a
        // worker's own spawn writes that name when the flag calls for it,
        // and the merge below leaves it in place.
        crate::spawn::apply_blocked_tools(&mut settings, spawn_kind, true);
        // The boot gate is a spawn precondition, not just a launchpad
        // decoration: a child stamped before the listener is bound
        // points at a base URL nothing answers.
        if !self.gateway_ready.load(std::sync::atomic::Ordering::Acquire) {
            return Err(forge_sdk::Error::Connection {
                reason: format!(
                    "the gateway listener on port {} is not ready; the boot gate is shut, so \
                     the session would be stamped at a base URL nothing serves",
                    self.gateway_port
                ),
            }
            .into());
        }
        // The caller may already have resolved the slot - the spawn
        // handlers do, because the bucket they announce under
        // `Spawning` has to be the one the child connects under.
        let session_slot = match resolved_key {
            Some(slot) => slot,
            None => self.resolve_slot(&target)?,
        };
        // The id the child runs under. `--session-id` and `--resume` are
        // the CLI's own arguments and stay ids; the store row records it
        // under the slot before the child starts, so a restart resolves
        // the same occupant from the same slot.
        let stored = self.stored_resume_id(&session_slot, settings.force_new)?;
        let resuming = stored.is_some();
        // One shared config dir: every account's child reads the same
        // MCP servers, plugins and settings, so the per-account dir is
        // gone along with the field that carried it.
        let account_dir = self.config_dir.clone();

        // Fast path: cache hit. A wake that resolved to a session already
        // in the pool hands back its handle; parked payloads are not a
        // concern here, because they are addressed to a slot rather than
        // to a key and this path never moved one.
        {
            let pool = self.pool.lock();
            if let Some(existing) = pool.get(&session_slot) {
                return Ok(Arc::clone(&existing.handle));
            }
        }

        // Resolve which account this spawn lands under: the walk over
        // the accounts declaring the project's model, which is the same
        // walk the gateway runs on a session's first request. Both the
        // project and its model are required - without a project there
        // is no org, no pin and no model for the walk to run over, and
        // an unregistered spawn would hand the child the account's real
        // credential and a direct base URL.
        let Some(project) = self.project_for_target(&target) else {
            return Err(
                WorkspaceError::SpawnResolvesToNoProject { slot: session_slot.display() }.into()
            );
        };
        // The role and the slot are two statements of the same thing -
        // the tool surface comes from the first and the address from the
        // second - so a caller that states them differently is refused
        // rather than handed a worker's address with a lead's tools.
        if Self::slot_for_spawn(role, &project) != session_slot {
            return Err(WorkspaceError::SpawnRoleSlotDisagree {
                role: role.clone(),
                slot: session_slot.display(),
            }
            .into());
        }
        // The gateway's walk is the one place a session's account is
        // decided: it looks the org's pin up, filters cooling accounts
        // and walks the accounts declaring the model. The spawn only
        // registers the answer, and that registration is what binds.
        let Some(model) = project.model.as_deref() else {
            return Err(WorkspaceError::ProjectModelMissing {
                project: project.name.clone(),
                org: project.org.clone(),
            }
            .into());
        };
        let account_key = self.select_account_for_project(&project, model)?;
        // The account and model checks have passed, so this spawn is
        // committed: record the id its lead will run under, either the
        // one the store held or the one minted in
        // `lead_session_key_for`. Nothing is written before this point -
        // a refusal ahead of it would pin an id no child ever adopts,
        // and the next boot would resume onto it.
        let session_id = self.record_spawn_id(&session_slot, stored);
        tracing::info!(
            target: "forge_workspace::account",
            slot = %session_slot.display(),
            session_id = %session_id,
            account = %account_key.0,
            "spawn bound to account",
        );

        // Slow path: spawn a fresh Agent bound to the workspace's
        // shared config_dir. The Agent stores it as a typed field;
        // every in-process accessor (oauth, settings, catalog scans)
        // reads it from there, and the spawned `claude` subprocess
        // inherits it as `CLAUDE_CONFIG_DIR` so each session reads/
        // writes the right account's user-data tree.
        let account_env = self.accounts.env(&account_key).unwrap_or_default();
        apply_project_permission_mode(Some(&project), &mut settings);
        let project_permission_mode = Some(project.permission_mode);
        // The project env merges BEFORE the gateway registers: the
        // stamp must land last, or a project env carrying a
        // base-url key would point the child away from the listener
        // while it still holds the dummy credential - the silent bypass
        // the stamp exists to close, reopened one layer up.
        let merged_env = session_env_for(&project, &account_env);
        // Register the session and let the gateway stamp the child's
        // env: the base URL names this listener, the credential the
        // child holds is a dummy, and the real one stays in the
        // gateway.
        let registration = self.accounts.provider(&account_key).map(|provider| {
            forge_gateway::binding::Registration {
                org: project.org.clone(),
                project: project.name.clone(),
                session: session_id.clone(),
                account: account_key.clone(),
                provider,
            }
        });
        let mut session_env = match &registration {
            Some(registration) => self
                .gateway
                .bindings
                .register(registration, &self.gateway_listener_url(), &merged_env)
                .into_map(),
            None => merged_env,
        };
        // The project's model fills the CLI's model slots: one model
        // for everything the CLI does - main, subagents, background
        // slots. A /model change re-seats the primary immediately; the
        // slots follow on the next respawn.
        if let Some(model) = project.model.as_ref() {
            for var in MODEL_SLOT_VARIABLES {
                session_env.insert((*var).to_owned(), model.clone());
            }
        }
        // Telling the CLI the project's model is what stops the
        // caller's pin - forge defaults it to the literal "opus" - from
        // resolving through an account's alias mapping into a model the
        // project never declared.
        apply_project_model(Some(&project), &mut settings);

        // Hoist DomainSession creation to BEFORE Agent::spawn. A caller
        // may have registered one at `session_key` already (the cron
        // and gotify delivery paths register a worker's domain before
        // its spawn); reuse it when present so the TUI's pre-spawn
        // accessors keep their handle reference. Otherwise create
        // fresh with conn = None and update post-Agent::spawn at line
        // ~470.
        let domain_arc = {
            let mut handles = self.domain_handles.lock();
            // A DomainSession is often already registered at
            // `session_key` (the cron and gotify delivery paths register
            // a worker's domain before dispatching its spawn). Reuse it,
            // so the TUI's pre-spawn accessors keep their handle
            // reference; otherwise create fresh with conn = None and fill
            // the handle in after `Agent::spawn`.
            if let Some(existing) = handles.get(&session_slot).cloned() {
                existing
            } else {
                let fresh = Arc::new(Mutex::new(DomainSession::new(session_slot.clone(), None)));
                handles.insert(session_slot.clone(), Arc::clone(&fresh));
                fresh
            }
        };
        // Carry `--new` onto the domain so a project lead's
        // Connected-time respawn skips the store lookup and brings its
        // workers up fresh alongside the fresh lead.
        domain_arc.lock().spawned_force_new = settings.force_new;
        // The effort this spawn asked for, so a session whose hook has
        // not reported one yet still answers with the level it runs at.
        domain_arc.lock().configured_effort =
            crate::domain_session::configured_effort_from_settings(&settings);
        // The mode, the same way: forge stamps its effective default into
        // the launch settings, so this is the mode the session was spawned
        // in rather than a guess.
        domain_arc.lock().configured_permission_mode =
            crate::domain_session::configured_permission_mode_from_settings(&settings);
        // Carry the row's provenance the same way: the tag-write
        // rollback runs long after this spawn returned and needs to know
        // whether the row it would delete is this spawn's to take.
        domain_arc.lock().spawn_wrote_row =
            matches!(role, crate::protocol::SpawnRole::Worker { wrote_row: true, .. });

        // Build the per-session `forge` MCP server. ONE server name;
        // tool surface depends on whether this spawn is for a project
        // lead or a worker. Both kinds reach any other session by its
        // slot; a lead additionally gets the four verbs that act on its
        // own project (spawn / despawn / update / capacity). See
        // `crate::mcp::SessionKind` for the rationale.
        // One source for both answers: the slot's label decides the kind,
        // so a worker's tool surface and a worker's address cannot
        // disagree.
        let session_kind = if session_slot.is_lead() {
            crate::mcp::SessionKind::Lead
        } else {
            crate::mcp::SessionKind::Worker
        };
        let forge_server = {
            let workspace_facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(self);
            let worker_facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(self);
            let browser_facade =
                crate::mcp::browser::facade::ProdBrowserFacade::from_workspace(self);
            let review_facade = crate::mcp::review::facade::ProdReviewFacade::from_arc(self);
            let cron_facade = crate::mcp::cron::facade::ProdCronFacade::from_arc(self);
            let gotify_facade = crate::mcp::gotify::facade::ProdGotifyFacade::from_arc(self);
            let slack_facade = crate::mcp::slack::facade::ProdSlackFacade::from_arc(self);
            let tasks_facade = crate::mcp::tasks::facade::ProdTasksFacade::from_arc(self);
            let systemone_facade = self.systemone.as_ref().map(|client| {
                crate::mcp::systemone::facade::ProdSystemOneFacade::new(Arc::clone(client))
                    .into_arc()
            });
            let families = match role {
                crate::protocol::SpawnRole::Lead => crate::mcp::McpFamily::all(),
                crate::protocol::SpawnRole::Worker { mcp_families, .. } => {
                    crate::mcp::resolve_mcp_families(mcp_families.as_deref())
                }
            };
            crate::mcp::build_forge_server(
                crate::mcp::ForgeServerFacades {
                    workspace: workspace_facade,
                    worker: worker_facade,
                    browser: browser_facade,
                    review: review_facade,
                    cron: cron_facade,
                    gotify: gotify_facade,
                    slack: slack_facade,
                    tasks: tasks_facade,
                    systemone: systemone_facade,
                },
                &families,
                session_slot.clone(),
                session_kind,
            )
        };

        // Derived once, so what a test records is the very value the spawn is
        // handed rather than a second reading of the slot.
        let worker_listing = self.worker_listing_for(&session_slot);
        #[cfg(any(test, feature = "testing"))]
        {
            *self.test_spawn_listing.lock() = match &worker_listing {
                Some(listing) => RecordedListing::Listed(listing.clone()),
                None => RecordedListing::NoListing,
            };
            // Kept beside the listing: the stand-in below replaces the call
            // the server would have been handed to, so this is the only way a
            // test sees the one the spawn composed.
            *self.test_spawn_server.lock() = Some(forge_server.clone());
        }
        let spawn_agent = || {
            forge_agent::Agent::spawn(
                account_dir.clone(),
                Some(account_key.0.clone()),
                vec![("forge".to_owned(), forge_server)],
                session_env,
                worker_listing,
            )
        };
        // A test that installed a stand-in reads the settings this spawn
        // hands its child, which nothing else can observe.
        #[cfg(any(test, feature = "testing"))]
        let handle = self.take_test_spawn_handle().unwrap_or_else(spawn_agent);
        #[cfg(not(any(test, feature = "testing")))]
        let handle = spawn_agent();
        // Project-rooted targets (`Default` / `Named`) resume the
        // project's lead session when the on-disk catalog has one,
        // and fall back to a fresh session in that project's cwd
        // otherwise. Pool key = lead's session id from the catalog
        // so it stays consistent with the running session id.
        // A project-rooted target (`Default` / `Named`) resumes the
        // project's lead session when its row holds an id, and starts a
        // fresh session under a minted one otherwise. The id is the
        // CLI's argument, so it is named here rather than derived.
        match target {
            SessionTarget::Default | SessionTarget::Named(_) => {
                let project = self.project_for_target(&target).ok_or_else(|| {
                    WorkspaceError::ProjectNotFound {
                        name: session_slot.project().to_owned(),
                        path: crate::config::forge_data_dir(&self.config_dir).join("forge.toml"),
                    }
                })?;
                Self::apply_lead_delegation(&mut settings, session_kind);
                let cwd = project.path.to_string_lossy().to_string();
                if resuming {
                    handle.resume_or_new_session(session_id.clone(), cwd, settings)?;
                } else {
                    handle.new_session(Some(session_id.clone()), cwd, settings)?;
                }
            }
            SessionTarget::Session(slot) => {
                let cwd = self.resume_cwd_for_slot(&slot);
                handle.resume_session(session_id.clone(), cwd, settings)?;
            }
            SessionTarget::FreshInProject { .. } => {
                // Worker spawn: a fresh session in the project's cwd.
                // Skip the lead-resume path so each worker runs under
                // the id its caller already minted and recorded.
                let cwd = project.path.to_string_lossy().to_string();
                handle.new_session(Some(session_id.clone()), cwd, settings)?;
            }
        }
        // The session is up; whatever is buffered on it belongs to the live
        // SessionTask to drain on Connected.
        let arc = Arc::new(handle);

        // Insert: race-safe via "if absent" semantics. If a concurrent
        // caller raced us to the spawn, theirs wins the pool slot and
        // ours drops at end-of-scope (subprocess killed via Client's
        // existing Drop). Single-user scope makes the race effectively
        // impossible.
        {
            let mut pool = self.pool.lock();
            if let Some(existing) = pool.get(&session_slot) {
                return Ok(Arc::clone(&existing.handle));
            }
            pool.insert(
                session_slot.clone(),
                PooledAgent {
                    handle: Arc::clone(&arc),
                    account: account_key.clone(),
                    permission_mode: project_permission_mode,
                    registration: registration.clone(),
                    session_id: session_id.clone(),
                },
            );
        }

        // Spawn the per-session `SessionTask` actor. Idempotent -
        // a second `get_agent_handle` call for the same key reuses
        // the existing task. The command channel is created only
        // on the cold path (no existing sender), held by the
        // workspace until the task takes its receiver.
        let cmd_rx = {
            let mut senders = self.command_senders.lock();
            if senders.contains_key(&session_slot) {
                None
            } else {
                let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
                senders.insert(session_slot.clone(), cmd_tx);
                Some(cmd_rx)
            }
        };
        if let Some(cmd_rx) = cmd_rx {
            // `domain_arc` was hoisted above the spawn. Stamp the live
            // `Arc<AgentHandle>` onto its `conn` slot
            // now that the handle exists. The TUI's pre-spawn
            // accessors (`connect::create_app`'s placeholder entry)
            // keep reading from the same `Arc<Mutex<...>>` they were
            // given before - no second `domain_session_for`
            // round-trip after the spawn lands.
            domain_arc.lock().conn = Some(Arc::clone(&arc));
            let domain = Arc::clone(&domain_arc);
            let task = SessionTask {
                key: session_slot,
                handle: Arc::clone(&arc),
                command_rx: cmd_rx,
                domain,
                update_tx: self.update_tx.clone(),
                // Every spawn now emits Connected on its first connect:
                // nothing replaces a live session's agent in-process any
                // more, so there is no SessionReplaced case to seed.
                connected_once: false,
                workspace: Arc::downgrade(self),
                // Filled by the first `Connected`, which is where this task
                // is handed the history it can later replay.
                conversation: None,
            };
            let span = tracing::info_span!(
                "session_task",
                slot = %task.key.display(),
            );
            tokio::spawn(task.run().instrument(span));
        }

        Ok(arc)
    }

    /// Crate-internal accessor for the boot-time loading task to
    /// drive `LoadingState` transitions on the workspace's account
    /// map. Not exposed beyond the crate - the field stays private
    /// so only intentional callers reach in.
    pub(crate) fn account_pool(&self) -> &forge_gateway::AccountPool {
        &self.accounts
    }

    /// The gateway's bind failure, if the listener could not start.
    /// The launchpad surfaces this instead of "loading accounts" so a
    /// dead bind does not read as an endless wait.
    pub fn gateway_bind_error(&self) -> Option<String> {
        self.gateway_bind_error.lock().clone()
    }

    /// Whether the gateway listener has bound its port. The preflight
    /// screen's gateway row renders this.
    pub fn gateway_ready(&self) -> bool {
        self.gateway_ready.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The port the gateway's listener binds.
    pub fn gateway_port(&self) -> u16 {
        self.gateway_port
    }

    /// The base URL children are stamped with: the listener's BOUND
    /// address, not the config-derived one - the config names the port
    /// that was asked for, the listener names the port that was bound,
    /// and the stamp must carry the one that answers.
    fn gateway_listener_url(&self) -> String {
        match self.gateway_url.lock().clone() {
            Some(url) => url,
            // Unreachable behind the boot gate: the gate is open only
            // after the listener stored its bound address.
            None => format!("http://127.0.0.1:{}", self.gateway_port),
        }
    }

    /// `true` when every `[[accounts]]` entry has reached a terminal
    /// `LoadingState` AND the gateway's listener is bound. The
    /// launchpad reads this to decide whether project rows are
    /// clickable - clicking while an account is still resolving means
    /// the walk runs over a partial map, and spawning before the
    /// listener is bound stamps a base URL nothing answers. Public so
    /// forge-tui can gate keyboard + mouse handlers on it without
    /// reaching into the crate-private account map.
    pub fn all_accounts_loaded(&self) -> bool {
        self.accounts.all_loaded() && self.gateway_ready.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether a spawn in `project_key` would find an account: the
    /// gateway's walk over the project's declared model, which is what a
    /// spawn runs and what the launchpad's chips show. `false` for a
    /// project without a model, or one whose org serves nothing it
    /// declares, which is the row the launchpad keeps unclickable.
    pub fn project_would_bind(&self, project_key: &ProjectKey) -> bool {
        let Some(project) = self.project_for_key(project_key) else {
            return false;
        };
        let Some(model) = project.model.as_deref() else {
            return false;
        };
        self.gateway.select_for(&project.org, model).is_ok()
    }

    /// Whether a spawn in `project_key` would be refused because every
    /// account serving its model is cooling.
    ///
    /// Matches the walk's budget failure alone, rather than taking any
    /// error, because that failure is the one carrying a reset to wait
    /// for - and only a refusal that clears on its own is owed a
    /// deferral. The walk's other failure is the config-level one, and
    /// `forge.toml` refuses a project whose model no account in its org
    /// serves (`ProjectModelUndeclared`), so it is not reachable from a
    /// loaded config today. Keep the match narrow anyway: an empty walk
    /// with nothing to wait for must fail as before rather than retry
    /// forever, and this is the line that says so.
    pub(crate) fn project_walk_is_cooling(&self, project_key: &ProjectKey) -> bool {
        let Some(project) = self.project_for_key(project_key) else {
            return false;
        };
        let Some(model) = project.model.as_deref() else {
            return false;
        };
        matches!(
            self.gateway.select_for(&project.org, model),
            Err(forge_gateway::SelectFailure::BudgetExhausted { .. })
        )
    }

    /// Snapshot of `(AccountKey display name, LoadingState)` pairs in
    /// declaration order. Forge-tui's launchpad renders the per-
    /// account loading glyph row from this; the order matches
    /// `forge.toml`'s `[[accounts]]` declarations so the glyphs sit
    /// next to the user's mental model of which-account-is-which.
    pub fn account_loading_snapshot(&self) -> Vec<AccountLoadingRow> {
        self.accounts
            .account_names()
            .into_iter()
            .map(|display_name| {
                let key = AccountKey(display_name.clone());
                let last_error = self.accounts.usage_error(&display_name);
                let retry_after = self.accounts.next_probe_after(&key);
                let auth = self.accounts.auth(&display_name);
                AccountLoadingRow {
                    display_name,
                    state: self.accounts.loading_state(&key),
                    last_error,
                    retry_after,
                    auth: auth.unwrap_or(crate::views::AccountAuth::Token),
                }
            })
            .collect()
    }

    /// How the account called `display_name` authenticates - the input
    /// the auth-repair hints branch on. `None` when the name isn't a
    /// configured account.
    pub fn account_auth_for(&self, display_name: &str) -> Option<crate::views::AccountAuth> {
        self.accounts.auth(display_name)
    }

    /// The account name back, when that account is saturated or bailed:
    /// the walk takes one of those only when nothing else in the pin
    /// declares the project's model, so a spawn landing there is worth
    /// saying out loud. `None` when it has room, and for an unknown name.
    pub(crate) fn degraded_account_name(&self, account: &str) -> Option<String> {
        let key = AccountKey(account.to_owned());
        let degraded = self.accounts.is_saturated(&key)
            || self.accounts.loading_state(&key) == forge_gateway::LoadingState::Bailed;
        degraded.then(|| account.to_owned())
    }

    /// The `forge.toml` this workspace loaded. Preflight names it as
    /// one of the two ways past an account that will not authenticate -
    /// the one that needs a restart, since config is read at boot. The
    /// other is repairing the account's own credentials, which a poller
    /// picks up in place without one.
    pub fn config_path(&self) -> PathBuf {
        crate::config::forge_data_dir(&self.config_dir).join("forge.toml")
    }

    /// The config dir this workspace owns: `forge.toml`'s parent dir, and
    /// the dir every session's child runs under, so a session's transcript
    /// lives in its `projects/` tree.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Per-model dictation progress for the preflight screen. Empty
    /// `models` means `[dictate] enabled` is false and preflight has no
    /// Dictation section to draw.
    pub fn dictate_snapshot(&self) -> crate::dictate::DictateSnapshot {
        self.dictate.snapshot.lock().clone()
    }

    /// Where the dictation models land. `None` when the platform has no
    /// usable cache directory and none was configured.
    pub fn dictate_models_dir(&self) -> Option<PathBuf> {
        self.config.dictate.models_dir()
    }

    /// The connection that was streaming a take for `key` has gone: the
    /// take is DROPPED, not submitted - its reader is gone, so nothing it
    /// produced would land anywhere, and stopping it now spends nothing
    /// further on it.
    ///
    /// Only a take by THIS connection is closed: a device take's audio
    /// comes from this machine and its recording task outlives any one
    /// client, and a take that ended with another one started since is
    /// not this connection's to close. The seat is free the moment this
    /// returns, so a client that reconnects and starts again is not
    /// refused by the take it left behind. Answers whether there was one
    /// to close.
    pub fn dictate_close(&self, key: &SessionSlot, initiator: u64) -> bool {
        let mut runtime = self.dictate_runtime.lock();
        let live = runtime
            .recordings
            .get(key)
            .is_some_and(|live| live.sink.is_some() && live.initiator == Some(initiator));
        if live {
            let Some(take) = runtime.recordings.remove(key) else {
                return false;
            };
            // `false` is the abandon the runner already honours for Esc:
            // the capture is released and its audio is transcribed no
            // further. A channel nobody is reading yet still takes the
            // value.
            let _ = take.stop.try_send(false);
            return true;
        }
        // A take already submitted is still this connection's to cancel:
        // its transcript would have no reader left either.
        if let Some(take) = runtime
            .finishing
            .iter()
            .find(|take| &take.key == key && take.initiator == Some(initiator))
        {
            let _ = take.stop.try_send(false);
            return true;
        }
        false
    }

    /// The dictate axes in force: `forge.toml` over the crate's own
    /// defaults. What a capturing client starts on and resets to.
    pub fn dictate_axes(&self) -> crate::dictate::DictateAxes {
        self.config.dictate.axes()
    }

    /// Push one frame of client-captured audio into the seat's live take,
    /// answering whether the samples were kept.
    ///
    /// **The take must be `initiator`'s own.** A frame carries no seat, and
    /// a seat's live take can be another connection's - decision 3's own
    /// scenario, where a client whose start was refused still has its
    /// microphone open for the frames already on the wire - so a push from
    /// any other connection is dropped rather than landing in someone
    /// else's dictation. `false` also means the seat has no live take
    /// (never started, refused, or already resolved) or the take has
    /// stopped (the cap, or the speaker let go). A caller drops the frame
    /// rather than holding it, because the take's own answer is what a
    /// reader sees either way.
    pub fn dictate_push(&self, key: &SessionSlot, samples: &[f32], initiator: Option<u64>) -> bool {
        let runtime = self.dictate_runtime.lock();
        runtime.frame_sink_for(key, initiator).is_some_and(|sink| sink.push_mono(samples))
    }

    /// The device this process records from, once a `/dictate` pick moved it.
    ///
    /// Volatile and process-wide rather than per session: a pick overrides the
    /// `[dictate] device` pin for every session until forge restarts, which is
    /// why it rides the dictate read rather than a session's state.
    pub fn dictate_device_pick(&self) -> Option<crate::dictate::DictateDeviceChoice> {
        self.dictate_device_pick.lock().clone()
    }

    /// The inputs a `/dictate` device pick can offer, plus the configured
    /// pin. Blocking (the cpal device walk), so the TUI calls it from a
    /// spawned task, never the render thread, and the socket answers its
    /// `devices` request with it off the connection's task.
    ///
    /// # Errors
    ///
    /// When the audio stack cannot be enumerated at all; the overlay
    /// renders the message in place of a list.
    pub fn dictate_device_catalog(&self) -> Result<crate::dictate::DictateDeviceCatalog, String> {
        let devices = forge_dictate::devices().map_err(|error| error.to_string())?;
        Ok(crate::dictate::DictateDeviceCatalog {
            devices,
            configured: self.config.dictate.device.clone(),
        })
    }

    /// Stop the in-flight model fetch. Whatever reached the disk stays
    /// there as a `.part`, so the next run resumes; the snapshot then
    /// carries [`crate::DictateFailure::Cancelled`] and forge quits,
    /// because there is no dictation-less runtime to fall back to.
    pub fn cancel_dictate_preflight(&self) {
        self.dictate.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Fetch, verify and load the dictation models. One task per forge
    /// run, started beside the account loaders; a no-op when dictation
    /// is switched off.
    ///
    /// The active models are resolved first - the config key, the runtime
    /// pick, or the compiled pin, per role - and the resolution's failure
    /// is the same stop a failed fetch is, naming the config key that
    /// could not be answered.
    pub fn start_dictate_preflight(self: &Arc<Self>) {
        let settings = self.config.dictate.clone();
        if !settings.enabled {
            return;
        }
        let state = Arc::clone(&self.dictate);
        let this = Arc::clone(self);
        let updates = self.update_sender();
        let span = tracing::info_span!("dictate_preflight");
        tokio::spawn(
            async move {
                let resolved = match this.resolve_active(&settings).await {
                    Ok(resolved) => resolved,
                    Err(message) => {
                        state.fail(crate::dictate::DictateFailure::Other { message }, None);
                        let models = this.dictate_models();
                        let _ = updates.send(SessionUpdate::DictateModelsChanged { models });
                        return;
                    }
                };
                state.set_active(&resolved);
                // The page's rows move with the resolution: a role the
                // config or a runtime pick names is not the pin it read a
                // moment ago.
                let models = this.dictate_models();
                let _ = updates.send(SessionUpdate::DictateModelsChanged { models });

                let cfg = crate::dictate::preflight_config(&settings, &resolved);
                crate::dictate::run_dictate_preflight(cfg, Arc::clone(&state)).await;
                // `run_dictate_preflight` parks the engine in the state
                // only on success, and every failure path ends the run,
                // so a held engine is the whole availability signal.
                if state.engine.lock().is_some() {
                    // A variant the config resolved from the feed is on
                    // disk now: record it so the page and the bench see it.
                    this.record_resolved_installs(&resolved);
                    let _ = updates.send(SessionUpdate::DictateAvailability);
                }
            }
            .instrument(span),
        );
    }

    /// Resolve `project_key` to the per-session chip the Projects pane
    /// renders next to each row: the account a spawn in it would land
    /// on, and that account's current visual-state category. Returns
    /// `None` when the project is unknown, declares no model, or its
    /// org serves nothing it declares.
    ///
    /// The walk is per org and model, so every row in a project shows
    /// the same account - which is what a spawn does.
    ///
    /// State derivation:
    /// - `LoadingState::Bailed` splits by the recorded failure class,
    ///   the same split the account rows render: auth failures
    ///   (`Unauthorized` / `Expired`) -> `SessionChipState::Bailed`
    ///   (red with a `⚠ ` prefix); transient classes and an unrecorded
    ///   bail -> `Degraded` (warning yellow, the pollers heal it).
    /// - `Ready` + any usage window at the cap (the same
    ///   `is_saturated` signal the walk demotes an account for) ->
    ///   `AtCap` (yellow; the session still spawns but will throttle).
    /// - Otherwise -> `Normal` (DIM; default chip).
    pub fn session_chip_for(&self, project_key: &ProjectKey) -> Option<SessionChipInfo> {
        let project = self.project_for_key(project_key)?;
        let model = project.model.as_deref()?;
        let account_key = self.gateway.select_for(&project.org, model).ok()?;

        let loading = self.accounts.loading_state(&account_key);
        let saturated = self.accounts.is_saturated(&account_key);
        let last_error = self.accounts.usage_error(&account_key.0);

        let state = match loading {
            forge_gateway::LoadingState::Bailed => match last_error {
                Some(
                    forge_gateway::UsageFetchStatus::Unauthorized
                    | forge_gateway::UsageFetchStatus::Expired,
                ) => SessionChipState::Bailed,
                _ => SessionChipState::Degraded,
            },
            forge_gateway::LoadingState::Ready if saturated => SessionChipState::AtCap,
            _ => SessionChipState::Normal,
        };

        Some(SessionChipInfo { account_name: account_key.0, state })
    }

    /// The account the gateway walks for a project's declared model,
    /// with the walk's failures turned into the vocabulary a spawn
    /// reports.
    fn select_account_for_project(
        &self,
        project: &LoadedProject,
        model: &str,
    ) -> Result<AccountKey, anyhow::Error> {
        use forge_gateway::SelectFailure;
        self.gateway.select_for(&project.org, model).map_err(|failure| match failure {
            SelectFailure::NoEligibleAccount { org, model } => {
                WorkspaceError::NoAccountServesProjectModel {
                    project: project.name.clone(),
                    org,
                    model,
                    accounts: project
                        .accounts
                        .iter()
                        .chain(project.fallback_accounts.iter())
                        .cloned()
                        .collect::<Vec<String>>()
                        .join(", "),
                }
                .into()
            }
            SelectFailure::BudgetExhausted { org, model, reset_in, .. } => {
                WorkspaceError::AllAccountsCooling {
                    project: project.name.clone(),
                    org,
                    model,
                    seconds: reset_in.as_secs(),
                }
                .into()
            }
            SelectFailure::UnknownOrg { org } => anyhow::anyhow!(
                "project '{}' names org '{org}', which the gateway holds no pin for",
                project.name
            ),
        })
    }

    /// Spawn one boot-time loading task per `[[accounts]]` entry in
    /// `forge.toml`. Each task runs the per-account loading state
    /// machine (in the crate-private `account_loader` module) until
    /// the account reaches a terminal `LoadingState` (`Ready` or
    /// `Bailed`). Called once at startup, replacing the older
    /// "single-probe-then-poll" boot model with the explicit loading
    /// gate that the launchpad now consults via
    /// `AccountStateMap::all_loaded()`.
    pub fn start_account_loading_tasks(self: &Arc<Self>) {
        for key in self.accounts.ordered_account_keys() {
            let span = tracing::info_span!("account_loading", account = %key.0);
            let weak = Arc::downgrade(self);
            tokio::spawn(
                async move {
                    crate::account_loader::run_account_loading(key, weak).await;
                }
                .instrument(span),
            );
        }
    }

    /// Spawn the 60 s background account-usage poller. Fetches
    /// OAuth usage for every `[[accounts]]` entry via the per-
    /// account config-dir's credentials file (no Agent spawn
    /// required), writes each result into `AccountStateMap.by_key`.
    /// The TUI's bottom panel and the gateway's account selection both
    /// read from that cache.
    ///
    /// Call once at construction, AFTER `start_account_loading_tasks`
    /// (which subsumed the old `spawn_initial_account_probe` in #246).
    /// A `usage_poller_started` flag guards against duplicate
    /// spawns - second and later calls return without spawning so a
    /// forge-tui programming error can't multiply the poll rate.
    /// Enqueue a worker kick on the dispatcher channel (#259). The
    /// drainer task fires one `Command::Prompt` per
    /// `KICK_DISPATCH_INTERVAL`, so simultaneous boot-time kicks
    /// across a team of N workers spread out as N × INTERVAL instead
    /// of all hitting Anthropic's per-IP burst limit in the same tick.
    ///
    /// Send errors (channel closed - workspace has shut down) are
    /// logged at `error` because they signal a kick was dropped after
    /// `maybe_kick_worker_on_connected` already decided to send. The
    /// worker stays idle in that case; this is rare in practice
    /// (workspace shutdown drains all sessions before the kick path
    /// could fire), but the log line preserves diagnosability.
    pub(crate) fn enqueue_kick(&self, request: KickRequest) {
        if let Err(err) = self.kick_dispatcher_tx.send(request) {
            tracing::error!(
                target: "forge_workspace::workspace",
                error = %err,
                "enqueue_kick: channel closed (workspace shutting down?); kick dropped",
            );
        }
    }

    /// Spawn the worker-kick drainer task (#259). Takes the receiver
    /// out of `kick_dispatcher_rx_slot` and starts a tokio task that
    /// loops on `recv()`, calls
    /// [`Self::dispatch_forged_prompt`] for each request, then sleeps
    /// `KICK_DISPATCH_INTERVAL` before the next pull.
    ///
    /// Call once at construction, AFTER `Workspace::new` returns and
    /// the result is Arc-wrapped (mirrors `start_account_loading_tasks`
    /// / `start_usage_poller` shape). Subsequent calls find the slot
    /// empty and no-op, so a forge-tui programming error can't
    /// duplicate the drainer.
    ///
    /// The drainer holds an `Arc::downgrade(self)` so the task exits
    /// cleanly when the workspace is dropped: each `upgrade()` returns
    /// `None` and the loop breaks. No explicit shutdown signal needed.
    pub fn start_kick_dispatcher(self: &Arc<Self>) {
        let Some(mut rx) = self.kick_dispatcher_rx_slot.lock().take() else {
            tracing::debug!(
                target: "forge_workspace::workspace",
                "start_kick_dispatcher called more than once; ignoring",
            );
            return;
        };
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("kick_dispatcher");
        tokio::spawn(
            async move {
                while let Some(req) = rx.recv().await {
                    let Some(workspace) = weak.upgrade() else {
                        return; // Workspace dropped; exit cleanly.
                    };
                    let session_key = req.slot.clone();
                    if let Err(err) =
                        workspace.dispatch_forged_prompt(&session_key, req.prompt_body)
                    {
                        tracing::error!(
                            target: "forge_workspace::workspace",
                            slot = %session_key.display(),
                            error = ?err,
                            "kick dispatcher: dispatch failed; kick dropped",
                        );
                    }
                    // Drop the Arc before sleeping so the workspace
                    // isn't held alive across the interval.
                    drop(workspace);
                    tokio::time::sleep(KICK_DISPATCH_INTERVAL).await;
                }
            }
            .instrument(span),
        );
    }

    /// Bind the gateway's inference listener and open the boot gate
    /// once the port is ours. Called once at boot, from the TUI's
    /// connect path. On success the launchpad's gate opens; on failure
    /// the gate STAYS SHUT and every spawn attempt
    /// are refused at the spawn entries with the reason attached - the
    /// refusal is the protection, not the log line.
    pub fn start_gateway_listener(self: &Arc<Self>) {
        let port = self.gateway_port;
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(workspace) = weak.upgrade() else { return };
            match forge_gateway::listener::GatewayListener::bind(port).await {
                Ok(listener) => {
                    *workspace.gateway_url.lock() = Some(listener.local_url());
                    workspace.gateway_ready.store(true, std::sync::atomic::Ordering::Release);
                    workspace.announce_accounts_changed();
                    tracing::info!(
                        target: "forge_workspace::workspace",
                        port,
                        "gateway listener bound",
                    );
                    let handler: Arc<dyn forge_gateway::listener::RouteHandler> =
                        workspace.gateway.clone();
                    listener.run(handler).await;
                }
                Err(error) => {
                    *workspace.gateway_bind_error.lock() = Some(error.to_string());
                    workspace.announce_accounts_changed();
                    tracing::error!(
                        target: "forge_workspace::workspace",
                        error = %error,
                        "the gateway listener could not start; the boot gate stays shut and \
                         spawn attempts are refused until forge restarts",
                    );
                }
            }
        });
    }

    pub fn start_usage_poller(self: &Arc<Self>) {
        if self.usage_poller_started.swap(true, std::sync::atomic::Ordering::AcqRel) {
            tracing::debug!(
                target: "forge_workspace::workspace",
                "start_usage_poller called more than once; ignoring",
            );
            return;
        }
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("usage_poller");
        tokio::spawn(
            async move {
                // Skip the immediate-fire tick: the boot-time loading
                // tasks (`start_account_loading_tasks`, #246) already
                // drove the live probes. Firing again right away would
                // burn another round of Anthropic-side per-IP rate-
                // limiter capacity for no gain. First tick of this
                // interval lands one USAGE_POLL_INTERVAL after boot.
                let mut interval = tokio::time::interval_at(
                    tokio::time::Instant::now() + USAGE_POLL_INTERVAL,
                    USAGE_POLL_INTERVAL,
                );
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(workspace) = weak.upgrade() else {
                        return; // Workspace dropped; exit cleanly.
                    };
                    workspace.refresh_account_usage_once().await;
                }
            }
            .instrument(span),
        );
    }

    /// One pass of the usage poller: fetch OAuth usage for every
    /// configured account in parallel, write each result back to
    /// `AccountStateMap`. Per-account fetch errors are logged at
    /// `warn` so persistent auth failures (revoked token, expired
    /// refresh, missing credentials file) surface in default log
    /// output. Snapshot-mapping errors stay at `debug` because they
    /// indicate a response-shape drift rather than something the
    /// user needs to act on. Public so tests can drive a
    /// deterministic refresh without waiting for the 30 s tick.
    pub async fn refresh_account_usage_once(self: &Arc<Self>) {
        // Skip accounts inside an active backoff window - a recent
        // probe failed and re-probing now would just re-trip the same
        // rate limit. `scheduler_should_probe` ORs the backoff gate
        // (`should_probe_now`) with the one-shot reset-clear override
        // hook (`has_just_cleared_cap_window`): cold-cache accounts
        // probe immediately, and a snapshot whose resets_at moment has
        // passed gets a fresh probe via the override even if the
        // backoff timer is still active.
        let entries = self.accounts.probe_entries();

        // Disarm any account where the override hook was the deciding
        // factor (should_probe_now was false but the hook said yes).
        // One-shot semantics: each successful probe arms the hook once
        // via set_usage; the override fires once, and subsequent
        // stale-reset state respects the backoff schedule until a
        // fresh snapshot lands.
        let due: Vec<AccountKey> = entries.iter().map(|(key, _, _)| key.clone()).collect();
        self.accounts.disarm_backoff_overrides(&due);
        // Sequential probes. Anthropic's `/api/oauth/usage` endpoint
        // has a per-IP burst limit; parallel spawns trip the limit
        // and produce HTTP 429s even well under the user's own quota.
        // Serial execution staggers requests by per-probe latency
        // (~hundreds of ms), within the 60 s poll interval.
        let mut any_success = false;
        for (key, provider, env) in entries {
            // The backend owns the probe; an auth failure surfaces and
            // the account stays bailed until the credential heals.
            let fetch_result = crate::provider_probe::probe_via_backend(provider, &env).await;
            match fetch_result {
                Ok(snapshot) => {
                    self.record_usage_success(&key, snapshot);
                    any_success = true;
                }
                Err(forge_gateway::ProbeError::Unmappable(message)) => {
                    self.accounts.set_last_error(
                        &key,
                        forge_gateway::UsageFetchStatus::Other,
                        None,
                    );
                    // This arm writes the pool too: `set_last_error` moves
                    // `last_error` and the re-probe instant, both of which
                    // cross on the account row. A pass whose only write is
                    // this one would otherwise announce nothing.
                    self.announce_accounts_changed();
                    tracing::debug!(
                        target: "forge_workspace::account",
                        account = %key.0,
                        error = %message,
                        "usage_poll snapshot mapping failed",
                    );
                }
                Err(err) => {
                    let status = classify_oauth_usage_error(&err);
                    // Pull the server-provided Retry-After out of the
                    // 429 variant so the next probe schedules against
                    // Anthropic's actual reset time rather than our
                    // local guess.
                    let retry_after = match &err {
                        forge_gateway::ProbeError::Fetch(
                            forge_primitives::usage::oauth::OauthUsageError::RateLimited {
                                retry_after,
                            },
                        ) => *retry_after,
                        _ => None,
                    };
                    self.accounts.set_last_error(&key, status, retry_after);
                    self.announce_accounts_changed();
                    // Branch the log message by error class so anyone
                    // reading triage logs gets the right framing.
                    // The previous shape said "persistent failures
                    // usually mean stale OAuth credentials" for every
                    // error variant - which misclassified 429
                    // rate-limits (a transient Anthropic throttle) as
                    // an auth/credentials issue and sent users hunting
                    // for /login problems that didn't exist.
                    let message: &str = match status {
                        forge_gateway::UsageFetchStatus::RateLimited => {
                            "usage_poll fetch rate-limited by Anthropic; sub-second Retry-After is treated as 'no hint' and we back off exponentially"
                        }
                        forge_gateway::UsageFetchStatus::Expired
                        | forge_gateway::UsageFetchStatus::Unauthorized => {
                            auth_repair_hint(provider)
                        }
                        forge_gateway::UsageFetchStatus::NetworkFailed => {
                            "usage_poll fetch failed with network error; will retry on next tick"
                        }
                        forge_gateway::UsageFetchStatus::Other => {
                            "usage_poll fetch failed with unhandled error class; see error field for details"
                        }
                    };
                    // A 429 is a healthy poller meeting its rate limit,
                    // which the backoff above already handles; the other
                    // classes mean the account's figures are missing for
                    // a reason someone may need to fix.
                    if matches!(status, forge_gateway::UsageFetchStatus::RateLimited) {
                        tracing::debug!(
                            target: "forge_workspace::account",
                            account = %key.0,
                            error = %err,
                            retry_after_secs = ?retry_after.map(|d| d.as_secs()),
                            status = ?status,
                            "{message}",
                        );
                    } else {
                        tracing::warn!(
                            target: "forge_workspace::account",
                            account = %key.0,
                            error = %err,
                            retry_after_secs = ?retry_after.map(|d| d.as_secs()),
                            status = ?status,
                            "{message}",
                        );
                    }
                }
            }
        }

        // Persist the snapshot to disk so the next forge launch's
        // launchpad picker has seed data even if Anthropic 429s the
        // first probe. Skipped when no probe succeeded this round -
        // no point rewriting the file with the same contents.
        if any_success {
            let snapshots = self.accounts.snapshots_for_cache();
            let account_count = snapshots.len();
            // A redb write is a small mmap'd transaction, not the old
            // load-merge-write of a TOML file, so it runs inline on the
            // held store handle like the cron + gotify writes do.
            if let Some(db) = self.db.lock().as_ref() {
                crate::account_cache::store(db, &snapshots);
            }
            tracing::info!(
                target: "forge_workspace::account_cache",
                event_name = "account_cache_written",
                accounts = account_count,
                "usage cache updated in the store after a successful poll round",
            );
        }
    }

    /// Write one successful poll result. A healed account is
    /// immediately eligible for new spawns: the walk reads the state
    /// map directly, so there is nothing to recompute. Existing
    /// sessions keep the account they bound to.
    pub(crate) fn record_usage_success(
        &self,
        key: &AccountKey,
        snapshot: forge_primitives::usage::UsageSnapshot,
    ) {
        use forge_gateway::LoadingState;
        let healed = self.accounts.loading_state(key) == LoadingState::Bailed;
        // A window at the cap with a reset ahead is proven exhaustion:
        // the gateway cools the account until that reset and every
        // bound session re-selects. Read off the incoming snapshot -
        // the pre-write pool state is the stale one.
        let probe_reset_at = snapshot
            .binding_reset_at()
            .and_then(|reset| reset.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        self.accounts.set_usage(key, snapshot);
        if let Some(reset_at) = probe_reset_at {
            self.gateway.report_probe_limit(key, Some(reset_at));
        }
        if healed {
            tracing::info!(
                target: "forge_workspace::account",
                event_name = "account_healed",
                account = %key.0,
                outcome = "ready",
                "Bailed -> Ready on a clean usage poll; the walk serves it for new sessions",
            );
        }
        self.announce_accounts_changed();
    }

    /// Read the cached usage snapshot for an account by display
    /// name. `None` when the poller hasn't yet succeeded (cold
    /// cache, no credentials, network blip). The TUI bottom panel
    /// renders the 5h / 7d bars from this snapshot.
    pub fn usage_for(&self, display_name: &str) -> Option<forge_primitives::usage::UsageSnapshot> {
        self.accounts.usage(display_name)
    }

    /// The account the gateway is serving `key`'s session with right
    /// now, which a re-selection may have moved off the spawn-time
    /// pick. The lookup goes through the pooled registration: bindings
    /// are keyed by the segment the child's base URL was stamped with,
    /// which is the id the occupant runs under.
    pub fn bound_account_for(&self, key: &SessionSlot) -> Option<AccountKey> {
        let (org, project, session) = {
            let pool = self.pool.lock();
            let registration = pool.get(key).and_then(|entry| entry.registration.as_ref())?;
            (registration.org.clone(), registration.project.clone(), registration.session.clone())
        };
        self.gateway.bindings.binding_for(&org, &project, &session)
    }

    /// Stamp a respawn's gateway registration for a child that runs
    /// under `session_id`: the four gateway-owned keys ride
    /// `launch_settings` as overrides, the binding moves to that
    /// segment, and the one it replaces is dropped. The pool entry
    /// records the same id, so `bound_account_for` reads the binding the
    /// child actually answers to. The gateway half is a no-op when the
    /// slot is not pooled or carries no registration: the launch then
    /// keeps the account env the original spawn laid down.
    ///
    /// The tools forge has replaced are denied unconditionally, because a
    /// respawn's settings are built by the TUI and carry no spawn-time
    /// flags - so `/new` and `/resume` are exactly where those denials
    /// would otherwise come back. A worker's own denials come back with
    /// them: the kind decides the worktree pins, and the row's own
    /// `interactive` flag decides the question.
    pub(crate) fn stamp_respawn_overrides(
        &self,
        slot: &SessionSlot,
        session_id: &str,
        launch_settings: &mut SessionLaunchSettings,
    ) {
        let kind = if slot.is_lead() {
            crate::mcp::SessionKind::Lead
        } else {
            crate::mcp::SessionKind::Worker
        };
        // A lead is the session whose row the user is looking at, so the
        // question is never denied to it. A worker's row answers for it,
        // and a row that never carried the flag was spawned under the
        // non-interactive default.
        let interactive = match kind {
            crate::mcp::SessionKind::Lead => true,
            crate::mcp::SessionKind::Worker => self.stored_interactive_for(slot).unwrap_or(false),
        };
        crate::spawn::apply_blocked_tools(launch_settings, kind, interactive);
        let (registration, replaced) = {
            let mut pool = self.pool.lock();
            let Some(entry) = pool.get_mut(slot) else { return };
            let Some(registration) = entry.registration.as_mut() else { return };
            let replaced = std::mem::replace(&mut registration.session, session_id.to_owned());
            let owned = registration.clone();
            session_id.clone_into(&mut entry.session_id);
            (owned, replaced)
        };
        launch_settings.env_overrides = self.gateway.bindings.respawn_env_overrides(
            &registration,
            &replaced,
            &self.gateway_listener_url(),
        );
    }

    /// Read the last poll-attempt failure for an account, if any.
    /// `None` when the most recent poll succeeded (or no attempt
    /// has been made yet). The TUI bottom panel renders a DIM hint
    /// next to the `5h` / `7d` label when this is `Some` so the
    /// user can tell an empty bar from an upstream failure (the
    /// HTTP 429 case is especially common when multiple forge
    /// instances poll the same Anthropic account).
    pub fn usage_error_for(&self, display_name: &str) -> Option<forge_gateway::UsageFetchStatus> {
        self.accounts.usage_error(display_name)
    }

    /// Snapshot the accounts a project may spawn under, in allow-list
    /// order, each carrying its live rate-limit state for the gateway
    /// view. `allowed_accounts` is the project's
    /// forge.toml pin; empty falls back to every configured account.
    /// `fallback_accounts`
    /// is the org's fallback list: unioned in (deduped) even when the
    /// pin does not name them, and flagged so a fallback-only row can
    /// carry its dim `fallback` suffix. Returns owned
    /// [`crate::AccountRow`]s so the TUI holds a snapshot rather than
    /// the `AccountStateMap` lock.
    pub fn project_accounts_snapshot(
        &self,
        allowed_accounts: &[String],
        fallback_accounts: &[String],
    ) -> Vec<crate::AccountRow> {
        // Resolve the allow-list to concrete account names, falling
        // back to every configured account when the project pins none.
        // Fallback names then join (deduped) - usually accounts the pin
        // does not name, since the org never rotates through them.
        let mut names: Vec<String> = if allowed_accounts.is_empty() {
            self.accounts.account_names()
        } else {
            allowed_accounts.to_vec()
        };
        for name in fallback_accounts {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        let mut rows: Vec<crate::AccountRow> = names
            .into_iter()
            .filter_map(|name| {
                let key = AccountKey(name.clone());
                let unusable = self.accounts.unusable_reason(&key);
                // A dual-listed account is primary-tier: the pin's
                // membership wins over the fallback list. An empty pin
                // means every account is primary (the un-pinned shape).
                let in_pin = allowed_accounts.is_empty() || allowed_accounts.contains(&name);
                let fallback = fallback_accounts.contains(&name) && !in_pin;
                let provider = self.accounts.provider(&key)?;
                let budget = account_budget(&name, provider, self.accounts.usage(&name).as_ref());
                Some(crate::AccountRow {
                    display_name: name,
                    unusable,
                    budget,
                    fallback,
                    provider,
                    loading: self.accounts.loading_state(&key),
                })
            })
            .collect();
        // Stable-sort the fallback group last, matching the gateway
        // view's group order. `false` sorts before `true`, and the sort
        // preserves within-group order.
        rows.sort_by_key(|row| row.fallback);
        rows
    }

    /// The gateway's own view, read-only: every org it holds, that
    /// org's walk order, and the live state of each account in it. A
    /// query refresh, not a `Command`.
    pub fn gateway_view_snapshot(&self) -> Vec<crate::views::GatewayOrgView> {
        self.gateway
            .org_pins()
            .into_iter()
            .map(|(org, pin)| {
                let rows = self.project_accounts_snapshot(&pin.accounts, &pin.fallback_accounts);
                crate::views::GatewayOrgView {
                    org,
                    accounts: pin.accounts,
                    fallback_accounts: pin.fallback_accounts,
                    rows,
                }
            })
            .collect()
    }

    /// Resolves a `SessionTarget` to the slot it names. A
    /// project-rooted target resolves to that project's lead slot; a
    /// specific session names its own. For `Named` with no matching
    /// project, returns [`WorkspaceError::ProjectNotFound`].
    pub(crate) fn resolve_slot(
        &self,
        target: &SessionTarget,
    ) -> Result<SessionSlot, WorkspaceError> {
        match target {
            SessionTarget::Default => {
                let project = self.config.default_project();
                Ok(SessionSlot::lead(&project.org, &project.name))
            }
            SessionTarget::Named(name) => {
                let project = self.find_project_by_name(name)?;
                Ok(SessionSlot::lead(&project.org, &project.name))
            }
            SessionTarget::Session(slot) | SessionTarget::FreshInProject { slot } => {
                Ok(slot.clone())
            }
        }
    }

    /// The project a spawn under `target` belongs to. A project-rooted
    /// target names it directly; a specific session slot names it by
    /// org and name.
    fn project_for_target(&self, target: &SessionTarget) -> Option<LoadedProject> {
        match target {
            SessionTarget::Default => Some(self.config.default_project().clone()),
            SessionTarget::Named(name) => self.find_project_view_by_name(name),
            SessionTarget::Session(slot) | SessionTarget::FreshInProject { slot } => self
                .config
                .projects
                .iter()
                .find(|project| project.org == slot.org() && project.name == slot.project())
                .cloned(),
        }
    }

    /// The project `slot` names, if `forge.toml` still declares it.
    pub(crate) fn project_for_slot(&self, slot: &SessionSlot) -> Option<LoadedProject> {
        self.config
            .projects
            .iter()
            .find(|project| project.org == slot.org() && project.name == slot.project())
            .cloned()
    }

    /// Look up a project by `name` from `forge.toml`. Returns
    /// [`WorkspaceError::ProjectNotFound`] when no project carries
    /// that name.
    fn find_project_by_name(&self, name: &str) -> Result<&LoadedProject, WorkspaceError> {
        self.config.projects.iter().find(|project| project.name == name).ok_or_else(|| {
            WorkspaceError::ProjectNotFound {
                name: name.to_owned(),
                path: crate::config::forge_data_dir(&self.config_dir).join("forge.toml"),
            }
        })
    }

    /// The charter the store holds for `slot`, if any. A worker's mission
    /// lives in its conversation, so a `/new` that emptied it has to
    /// re-deliver this or the worker comes back with no idea what it is
    /// for. Only a worker has one, and a lead's absence is the ordinary
    /// answer here.
    pub(crate) fn stored_charter_for(&self, slot: &SessionSlot) -> Option<String> {
        let db = self.db.lock();
        let db = db.as_ref()?;
        crate::store::sessions::get(db, slot.org(), slot.project(), slot.label())
            .ok()
            .flatten()
            .and_then(|row| row.charter)
    }

    /// Whether the store's row for `slot` says its session may ask its user
    /// a question. `None` when the store holds no row: a lead has none, and
    /// a worker without one was never spawned.
    pub(crate) fn stored_interactive_for(&self, slot: &SessionSlot) -> Option<bool> {
        let db = self.db.lock();
        let db = db.as_ref()?;
        crate::store::sessions::get(db, slot.org(), slot.project(), slot.label())
            .ok()
            .flatten()
            .and_then(|row| row.interactive)
    }

    /// The id the store holds for `slot`, unless `--new` (`force_new`)
    /// forces a fresh session. `Ok(Some(id))` => resume that id;
    /// `Ok(None)` => start fresh under a minted one. `force_new`
    /// overrides a stored id - that is what makes the boot wave's leads
    /// come up fresh under `--new`, while every non-boot spawn leaves
    /// `force_new` false and resumes.
    ///
    /// The store is the only source. A row with no id is a session that
    /// has never run, so the caller mints rather than re-deriving one
    /// from the transcripts. A store that cannot be read is an error
    /// rather than a mint: the session it names would be forked, and the
    /// new id written over the row that could not be read.
    fn stored_resume_id(
        &self,
        slot: &SessionSlot,
        force_new: bool,
    ) -> Result<Option<String>, WorkspaceError> {
        if force_new {
            return Ok(None);
        }
        match self.stored_session_id(slot.org(), slot.project(), slot.label()) {
            Ok(id) => Ok(id),
            Err(source) => {
                tracing::error!(
                    target: "forge_workspace::sessions",
                    org = slot.org(),
                    project = slot.project(),
                    label = slot.label(),
                    %source,
                    "reading the session row failed; refusing the spawn rather than minting over it",
                );
                Err(WorkspaceError::SessionStoreUnreadable {
                    org: slot.org().to_owned(),
                    project: slot.project().to_owned(),
                    label: slot.label().to_owned(),
                })
            }
        }
    }

    /// Record `id` as the session for `slot`, minting a fresh one when
    /// the caller has none to hand in.
    fn record_spawn_id(&self, slot: &SessionSlot, id: Option<String>) -> String {
        let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.record_session_id(slot.org(), slot.project(), slot.label(), &id);
        id
    }

    /// Record the id the CLI adopted for a live session, so the store
    /// stops disagreeing with what is running. An in-session `/resume`, a
    /// `/clear`, a login or a logout can move a session's id without
    /// forge choosing it, and a boot resolves a session from this row.
    ///
    /// `slot` is the one the task is registered under, so the row is
    /// named rather than derived: a session's role is what its spawn
    /// stated and nothing re-derives it from the live-worker registry.
    pub(crate) fn note_running_session_id(&self, slot: &SessionSlot, session_id: &str) {
        self.note_worker_session_id(slot, session_id);
        let stored = match self.stored_session_id(slot.org(), slot.project(), slot.label()) {
            Ok(stored) => stored,
            Err(error) => {
                tracing::error!(
                    target: "forge_workspace::sessions",
                    org = slot.org(),
                    project = slot.project(),
                    label = slot.label(),
                    %error,
                    "reading the session row failed; not recording the adopted id over it",
                );
                return;
            }
        };
        if stored.as_deref() == Some(session_id) {
            return;
        }
        tracing::info!(
            target: "forge_workspace::sessions",
            org = slot.org(),
            project = slot.project(),
            label = slot.label(),
            session_id,
            "recording the id the CLI adopted, which is not the one the store held",
        );
        self.record_session_id(slot.org(), slot.project(), slot.label(), session_id);
    }

    /// Mirror the id a live worker adopted onto its registry entry, so
    /// the status echo and `agents__list` name the occupant that is
    /// running rather than the one it was spawned under.
    fn note_worker_session_id(&self, slot: &SessionSlot, session_id: &str) {
        if let Some(entry) = self.pool.lock().get_mut(slot) {
            session_id.clone_into(&mut entry.session_id);
        }
        for entry in self.live_workers.lock().values_mut().flatten() {
            if entry.slot == *slot {
                entry.session_id = Some(forge_primitives::SessionId::new(session_id));
            }
        }
    }

    /// The id the session at `slot` runs under, when this process holds
    /// it live. `None` for a slot with no pooled session. This is the
    /// slot's occupant rather than a snapshot of when a caller last
    /// heard about it, so a read that has to name the id now - a
    /// transcript path, a CLI argument - asks here.
    pub fn running_session_id_for(&self, slot: &SessionSlot) -> Option<String> {
        self.pool.lock().get(slot).map(|entry| entry.session_id.clone())
    }

    /// Whether an agent is currently pooled for `slot`.
    pub(crate) fn session_is_pooled(&self, slot: &SessionSlot) -> bool {
        self.pool.lock().contains_key(slot)
    }

    /// When the transcript of the session at `slot` was last written,
    /// read from the catalog row naming the id it currently runs under.
    /// `None` for a slot with no live session, or one whose transcript
    /// is not on disk yet.
    pub fn session_last_activity(&self, slot: &SessionSlot) -> Option<std::time::SystemTime> {
        let id = self.running_session_id_for(slot)?;
        self.catalog
            .lock()
            .values()
            .flatten()
            .find(|info| info.session_id == id)
            .map(|info| UNIX_EPOCH + Duration::from_millis(info.last_modified))
    }

    /// A fresh id for a session starting now, recorded against `slot`'s
    /// row.
    pub(crate) fn fresh_session_id_for(&self, slot: &SessionSlot) -> String {
        self.record_spawn_id(slot, None)
    }

    /// The session id the store holds for `(org, project, label)`.
    ///
    /// `Ok(None)` means no id: the row is absent, or it carries none.
    /// That is a session which has never run, and a caller may mint for
    /// it. `Err` means the row could not be read, which is a different
    /// answer and must never be treated as absence: a caller that minted
    /// there would fork the session and write the new id over the row it
    /// failed to read.
    fn stored_session_id(
        &self,
        org: &str,
        project: &str,
        label: &str,
    ) -> Result<Option<String>, anyhow::Error> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else {
            // No store at all, so there is no row to read and none a mint
            // could overwrite.
            return Ok(None);
        };
        Ok(crate::store::sessions::get(db, org, project, label)?.and_then(|row| row.session_id))
    }

    /// Record `id` as the session for `(org, project, label)`, keeping the
    /// fields a worker's row already carries. A write that cannot land is
    /// warned and not retried: the session still runs under `id`, and the
    /// next boot reads whatever the row held.
    pub(crate) fn record_session_id(&self, org: &str, project: &str, label: &str, id: &str) {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return };
        let existing = crate::store::sessions::get(db, org, project, label).ok().flatten();
        let row = crate::store::sessions::SessionRecord {
            org: org.to_owned(),
            project: project.to_owned(),
            label: label.to_owned(),
            session_id: Some(id.to_owned()),
            charter: existing.as_ref().and_then(|row| row.charter.clone()),
            kick: existing.as_ref().and_then(|row| row.kick.clone()),
            resume_kick: existing.as_ref().and_then(|row| row.resume_kick.clone()),
            interactive: existing.as_ref().and_then(|row| row.interactive),
            is_git_repo: existing.as_ref().and_then(|row| row.is_git_repo),
            mcp_families: existing.as_ref().and_then(|row| row.mcp_families.clone()),
        };
        if let Err(error) = crate::store::sessions::put(db, &row) {
            tracing::warn!(
                target: "forge_workspace::sessions",
                org, project, label, session_id = %id, %error,
                "recording the session id failed; it will not survive a restart",
            );
        }
    }

    /// Locate a `ProjectView`-like (`LoadedProject`) by `name` from
    /// `forge.toml`. Returns `None` when no project carries that name.
    /// Used by the spawn handlers to resolve the project's path / cwd
    /// before emitting `SessionUpdate::Spawning`.
    pub(crate) fn find_project_view_by_name(&self, name: &str) -> Option<LoadedProject> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(found) =
            self.test_extra_projects.lock().iter().find(|p| p.name == name).cloned()
        {
            return Some(found);
        }
        self.config.projects.iter().find(|p| p.name == name).cloned()
    }

    /// The seat a project-scoped section update routes on: the project's
    /// lead, which exists for every declared project. `None` for a name no
    /// project carries, the shape a project that left forge.toml leaves.
    pub(crate) fn lead_slot_for_project(&self, project_name: &str) -> Option<SessionSlot> {
        self.find_project_view_by_name(project_name)
            .map(|project| SessionSlot::lead(project.org, project.name))
    }

    /// Internal accessor for the SessionUpdate fan-in sender. Used
    /// by `spawn.rs` to emit `Spawning` / `ConnectionFailed` /
    /// `FatalError` from the App-level handlers.
    pub(crate) fn update_tx(&self) -> &UpdateFanout {
        &self.update_tx
    }

    /// Tell every view the account pool moved.
    ///
    /// Called from the writes that move it - a boot probe settling, the usage
    /// poller landing or failing, and the listener binding - because none of
    /// them belongs to a seat and the pool's state is a snapshot field with no
    /// stream of its own.
    pub(crate) fn announce_accounts_changed(&self) {
        let _ = self.update_tx.send(SessionUpdate::AccountsChanged);
    }

    /// The cwd a project lead's slot runs in: its project's path.
    /// `claude --resume` indexes by the project key derived from the
    /// subprocess's working directory, so every explicit-resume code
    /// path must spawn the subprocess in the session's original cwd or
    /// the resume hits "No conversation found with session ID ..." even
    /// when the `.jsonl` exists.
    ///
    /// Read from the slot rather than from the catalog: a catalog row is
    /// keyed by the id the CLI adopted, which `/new` replaces, so the
    /// lookup answered nothing for the session that had just started and
    /// resume was handed an empty working directory.
    ///
    /// A WORKER answers nothing here, deliberately. Its cwd is its
    /// worktree, which only the live-worker registry can compose; the
    /// project root is a plausible-looking wrong answer that would shadow
    /// the registry arm in [`Self::cwd_for_session`] and send a git
    /// worker's resume to the wrong directory.
    fn session_cwd_for(&self, slot: &SessionSlot) -> Option<String> {
        if !slot.is_lead() {
            return None;
        }
        Some(self.project_for_slot(slot)?.path.to_string_lossy().into_owned())
    }

    /// Insert (or update) a session entry under the project that
    /// owns `cwd`. Called by the forge-tui connect-flow after every
    /// `Connected` migration so newly spawned sessions surface in the
    /// Projects-pane drilldown immediately, without forcing a full
    /// disk re-scan. The entry is placed at the head of the
    /// project's session list to match the most-recent-first ordering
    /// the scan produces.
    pub fn record_connected_session(&self, cwd: &str, session_id: &str, summary: Option<String>) {
        let storage_key =
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some(cwd));
        let key = ProjectKey::new(storage_key.clone());
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let entry = SDKSessionInfo {
            session_id: session_id.to_owned(),
            summary: summary.unwrap_or_else(|| "new session".to_owned()),
            last_modified: now_ms,
            file_size: None,
            custom_title: None,
            first_prompt: None,
            git_branch: None,
            cwd: Some(cwd.to_owned()),
            storage_key,
            tag: None,
            created_at: None,
        };
        let mut catalog = self.catalog.lock();
        let entries = catalog.entry(key).or_default();
        entries.retain(|e| e.session_id != session_id);
        entries.insert(0, entry);
    }

    /// Subscribe to this workspace's [`SessionUpdate`] stream. Every
    /// caller gets its own stream, so a second view attaches beside the
    /// first rather than being refused, and the workspace drops the
    /// matching subscription once its receiver goes.
    ///
    /// A stream carries what is emitted after this call. The first
    /// caller to attach is handed whatever the workspace emitted before
    /// it as well, which is how a notice raised during boot reaches a
    /// view; a caller attaching after one already has inherits no
    /// backlog.
    ///
    /// The stream answers the workspace's prompts, so a permission or
    /// question request delivered here keeps its turn alive waiting for
    /// the reply. A consumer that only reads the stream takes
    /// [`Self::subscribe_observer`] instead.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<SessionUpdate> {
        self.update_tx.subscribe(SubscriberRole::Answering)
    }

    /// Subscribe without answering the workspace's prompts: a logger, a
    /// mirror, anything that reads the stream but renders no prompt.
    ///
    /// The distinction is load-bearing rather than descriptive. A
    /// permission, question or Slack-draft request is parked on a reply,
    /// and the paths that raise one resolve it `Cancelled` instead when
    /// no subscriber can answer, so that a request nobody will reply to
    /// fails the turn rather than hanging it. A subscriber that declares
    /// itself an observer is not counted as an answer.
    ///
    /// **A browser hand-off fails closed differently**: nothing parks a
    /// decision for it to cancel, so its registration hands the blocked call
    /// a dead receiver instead and the tool answers the reason - no attached
    /// client can show the hand-off - rather than holding a session on a
    /// prompt no view can draw.
    ///
    /// Who takes the pre-attach backlog is positional, not role-aware:
    /// an observer subscribing before the TUI is the first caller and
    /// takes whatever the workspace emitted beforehand, the boot notice
    /// included. The binary's `start_*` calls all run before the TUI
    /// attaches, so this belongs after them rather than among them.
    ///
    /// Nothing calls this yet. The TUI is the only frontend and it
    /// answers, so the only observer in the tree is a test; this exists
    /// so a consumer that reads without rendering a prompt can say so
    /// instead of parking a turn by claiming a capability it lacks.
    pub fn subscribe_observer(&self) -> mpsc::UnboundedReceiver<SessionUpdate> {
        self.update_tx.subscribe(SubscriberRole::Observing)
    }

    /// Subscribe as an observer, carrying only what is emitted from here
    /// on: a mirror of the stream, not a renderer of its prompts.
    ///
    /// The backlog goes to the first subscriber, so a mirror attaching at
    /// boot must not take it - the view that renders prompts would lose
    /// the boot notice. A mirror reads the rest of what it needs from the
    /// read surface when it is asked, so it has nothing to replay.
    pub fn subscribe_mirror(&self) -> mpsc::UnboundedReceiver<SessionUpdate> {
        self.update_tx.subscribe_without_backlog(SubscriberRole::Observing)
    }

    /// Subscribe as an answerer, carrying only what is emitted from here on.
    ///
    /// The answering half of [`Self::subscribe_mirror`], for a client that
    /// renders prompts and attaches beside a terminal: it can answer, and the
    /// backlog still belongs to whoever was there first, because that is the
    /// view drawing the boot notice.
    pub fn subscribe_answerer(&self) -> mpsc::UnboundedReceiver<SessionUpdate> {
        self.update_tx.subscribe_without_backlog(SubscriberRole::Answering)
    }

    /// Clone the workspace's [`SessionUpdate`] sender. Internal to this
    /// crate: a view's own async work belongs on a channel of its own,
    /// so that [`Self::subscribe`] is the whole of what the core owes a
    /// frontend.
    pub(crate) fn update_sender(&self) -> UpdateFanout {
        self.update_tx.clone()
    }

    /// Borrow the workspace-side [`DomainSession`] for `key`.
    ///
    /// Returns the [`Arc`]-cloned mutex protecting the domain bucket.
    /// Callers `.lock()` to read or mutate. `None` when no
    /// `SessionTask` is registered for `key` (e.g., the session was
    /// closed or hasn't been spawned yet).
    ///
    /// Callers should hold the lock for the shortest scope possible;
    /// concurrent reducers and the per-session `SessionTask` share
    /// this mutex.
    pub fn domain_session_for(&self, key: &SessionSlot) -> Option<Arc<Mutex<DomainSession>>> {
        self.domain_handles.lock().get(key).cloned()
    }

    /// The slash commands the CLI last advertised for the session at
    /// `slot`. Empty for a slot whose session has not connected yet, or
    /// whose CLI advertised none.
    pub fn available_commands_for(&self, slot: &SessionSlot) -> Vec<AvailableCommand> {
        self.domain_session_for(slot)
            .map(|domain| domain.lock().available_commands.clone())
            .unwrap_or_default()
    }

    /// The subagents the CLI last advertised for the session at `slot`.
    /// Empty in the same two cases as [`Self::available_commands_for`].
    pub fn available_agents_for(&self, slot: &SessionSlot) -> Vec<AvailableAgent> {
        self.domain_session_for(slot)
            .map(|domain| domain.lock().available_agents.clone())
            .unwrap_or_default()
    }

    /// Whether the conversation at `slot` dispatched a sub-agent, anywhere in
    /// it. Raised by the session task's fold, which is what announces the
    /// raise, so a record reading this and a viewer hearing it agree.
    pub fn has_dispatches_for(&self, slot: &SessionSlot) -> bool {
        self.domain_session_for(slot).is_some_and(|domain| domain.lock().has_dispatches)
    }

    /// The sub-agent instances the session at `slot` has folded, as the list
    /// the session task pushes when it moves.
    ///
    /// **A record reads this rather than folding the conversation itself.**
    /// The task folds a card per frame and announces the list moving, so a
    /// second fold over the held conversation would be a second answer to a
    /// question one fold already answers - and one that disagrees with the
    /// frames as soon as the conversation a record can see is shorter than the
    /// one the session has run.
    pub fn subagent_cards_for(
        &self,
        slot: &SessionSlot,
    ) -> Vec<forge_primitives::runtime::SubagentCard> {
        self.domain_session_for(slot)
            .map(|domain| domain.lock().cards_snapshot.clone())
            .unwrap_or_default()
    }

    /// Whether the session at `key` currently has a live agent
    /// handle stamped onto its [`DomainSession`]. Encapsulates the
    /// presence check so callers don't need to peek at
    /// `DomainSession.conn` directly - the field layout is a
    /// workspace internal.
    pub fn has_agent_for(&self, key: &SessionSlot) -> bool {
        self.domain_session_for(key).is_some_and(|d| d.lock().conn.is_some())
    }

    /// Apply a `/dictate` override edit to the session's `DomainSession`
    /// and echo the full set back. An unknown session is refused the same
    /// way the per-session commands are: there is nothing to edit.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::UnknownSession`] when no `DomainSession`
    /// is registered for the key.
    fn apply_dictate_override(
        self: &Arc<Self>,
        key: &SessionSlot,
        update: crate::dictate::DictateOverrideUpdate,
    ) -> Result<(), DispatchError> {
        let Some(domain) = self.domain_session_for(key) else {
            return Err(DispatchError::UnknownSession(key.clone()));
        };
        match update {
            crate::dictate::DictateOverrideUpdate::Styling(v) => {
                domain.lock().dictate_overrides.styling = Some(v);
            }
            crate::dictate::DictateOverrideUpdate::Structure(v) => {
                domain.lock().dictate_overrides.structure = Some(v);
            }
            crate::dictate::DictateOverrideUpdate::Context(v) => {
                domain.lock().dictate_overrides.context = Some(v);
            }
            crate::dictate::DictateOverrideUpdate::Reset => {
                domain.lock().dictate_overrides = crate::dictate::DictateOverrides::default();
                *self.dictate_device_pick.lock() = None;
            }
        }
        let overrides = domain.lock().dictate_overrides;
        let pick = self.dictate_device_pick.lock().clone();
        let _ = self
            .update_sender()
            .send(SessionUpdate::DictateOverrides { key: key.clone(), overrides });
        let _ =
            self.update_sender().send(SessionUpdate::DictateDevicePin { key: key.clone(), pick });
        Ok(())
    }

    /// Apply a `/dictate` device pick (or its clear) and echo it. The
    /// pick is workspace state shared by every session - it overrides
    /// the configured pin for every capture until the process ends.
    fn apply_dictate_device(
        self: &Arc<Self>,
        key: &SessionSlot,
        pick: Option<crate::dictate::DictateDeviceChoice>,
    ) -> Result<(), DispatchError> {
        if self.domain_session_for(key).is_none() {
            return Err(DispatchError::UnknownSession(key.clone()));
        }
        (*self.dictate_device_pick.lock()).clone_from(&pick);
        let _ =
            self.update_sender().send(SessionUpdate::DictateDevicePin { key: key.clone(), pick });
        Ok(())
    }

    /// Dispatch a workspace-originated plain prompt (cron fire, peer,
    /// gotify or slack delivery, kick, notices), signalling
    /// `PromptQueuedWhileBusy` first when the target's turn is in
    /// flight so the TUI bridges the spinner across the gap.
    ///
    /// **A delivery drawn in a view needs an update here, paired with an arm
    /// in `forge-server`'s `delivery_turn`.** This path emits no frame of its
    /// own - the forge for one is `delivery_turn`, downstream, and it ends in
    /// `_ => return None`. So a prompt sent from here whose typed update has
    /// no arm there draws nothing in any view, silently: a prompt with no
    /// envelope of its own takes [`Self::dispatch_forged_prompt`] instead,
    /// which forges the frame here.
    pub fn dispatch_workspace_prompt(
        self: &Arc<Self>,
        key: &SessionSlot,
        text: String,
    ) -> Result<(), DispatchError> {
        self.dispatch_workspace_prompt_from(key, text, PromptSource::Forge)
    }

    /// [`Self::dispatch_workspace_prompt`], with the row's source label named
    /// by the caller: a delivery knows what it is (a cron fire, a Gotify
    /// notification, a peer comm) and the queued row has to say so.
    pub fn dispatch_workspace_prompt_from(
        self: &Arc<Self>,
        key: &SessionSlot,
        text: String,
        source: PromptSource,
    ) -> Result<(), DispatchError> {
        self.dispatch_workspace_prompt_under(
            key,
            text,
            source,
            forge_sdk::request_id::next_prompt_id(),
        )
    }

    /// [`Self::dispatch_workspace_prompt_from`] with the id supplied by the
    /// caller, for a site that already minted one: a delivery that draws its
    /// own envelope row passes the same id to both, so the row a view holds
    /// while the prompt queues and the prompt's lifecycle frames are one thing
    /// by id.
    pub fn dispatch_workspace_prompt_under(
        self: &Arc<Self>,
        key: &SessionSlot,
        text: String,
        source: PromptSource,
        uuid: String,
    ) -> Result<(), DispatchError> {
        // Busy is captured before the dispatch: dispatching first would
        // read the turn_pending stamp the dispatch itself just set.
        // Signalling only on success keeps a failed dispatch (the
        // log-only failure sites never emit a TurnError) from
        // stranding a count nothing clears. Whether the re-open-gap
        // residual signals at all depends on a session_state_changed
        // mirror being present, so it is CLI-version-dependent.
        let busy = self.domain_session_for(key).is_some_and(|d| d.lock().turn_in_flight());
        // `route` rather than `dispatch`: the delivery's frame is the
        // envelope one its own update forges, not a bare user turn.
        let result = self.route(Command::PromptUnder {
            key: key.clone(),
            text,
            attachments: Vec::new(),
            uuid,
            source,
        });
        if busy && result.is_ok() {
            let _ = self
                .update_sender()
                .send(SessionUpdate::PromptQueuedWhileBusy { key: key.clone() });
        }
        result
    }

    /// Send the continuation prompt for every seat whose failed turn is
    /// still unwatched and whose delay has run out (#1841).
    ///
    /// `now` is injected the way [`Self::fire_due_crons`] injects it, so a
    /// test steps the clock instead of waiting on one. Firing is one prompt
    /// per unopened failure: the delay is not a retry ladder, and a failure
    /// that keeps failing is nudged once rather than in a loop.
    pub fn fire_due_auto_continues(self: &Arc<Self>, now: SystemTime) {
        let keys: Vec<SessionSlot> = self.domain_handles.lock().keys().cloned().collect();
        for key in keys {
            let due = self.domain_session_for(&key).is_some_and(|domain| {
                domain.lock().auto_continue.as_ref().is_some_and(|pending| pending.due_at <= now)
            });
            if due {
                self.fire_auto_continue(&key);
            }
        }
    }

    /// One seat's nudge, refused when the failure is the reader's to see
    /// first. The decision is taken under the lock; the dispatch is not.
    fn fire_auto_continue(self: &Arc<Self>, key: &SessionSlot) {
        // The hold is read and released before the session lock is taken, so
        // the two never nest; `hold_seat` writes the stamp before taking the
        // hold, so reading both here covers the window between them.
        let held = self.held_work_seats.is_held(key);
        let Some(domain) = self.domain_session_for(key) else {
            return;
        };
        let reason = {
            let mut guard = domain.lock();
            // Spent either way: the decision for this failure is made here,
            // so a burst of sweeps cannot send the same nudge twice.
            let Some(pending) = guard.auto_continue.take() else {
                return;
            };
            let Some(failed_at) = guard.failed_turn_at else {
                return;
            };
            // #1612's boundary, one rule: a seat a view is holding is being
            // watched, and one shown since the failure has already been
            // looked at.
            if held || guard.shown_at.is_some_and(|shown| shown >= failed_at) {
                return;
            }
            guard.auto_continue_spent = true;
            pending.reason
        };
        let text = auto_continue_prompt(&reason);
        if let Err(error) = self.dispatch_forged_prompt(key, text) {
            tracing::warn!(
                target: "forge_workspace::workspace",
                event_name = "auto_continue_dispatch_failed",
                slot = %key.display(),
                %error,
                "the continuation prompt for a failed turn could not be dispatched",
            );
            return;
        }
        tracing::info!(
            target: "forge_workspace::workspace",
            event_name = "auto_continued_failed_turn",
            slot = %key.display(),
            "nudged a failed turn nobody had looked at",
        );
    }

    /// Watch for a seat whose failed turn nobody has looked at and nudge it
    /// once its delay has run out. Idempotent, started once at boot beside
    /// the other core tasks.
    pub fn start_auto_continue_sweep(self: &Arc<Self>) {
        if self.auto_continue_sweep_started.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("auto_continue_sweep");
        tokio::spawn(
            async move {
                loop {
                    tokio::time::sleep(AUTO_CONTINUE_SWEEP_INTERVAL).await;
                    let Some(workspace) = weak.upgrade() else {
                        return;
                    };
                    workspace.fire_due_auto_continues(SystemTime::now());
                }
            }
            .instrument(span),
        );
    }

    /// Dispatch a workspace-originated prompt whose words no view has drawn:
    /// a worker kick, an auto-continue. [`Self::dispatch_workspace_prompt`]
    /// exactly, plus the frame - nothing else carries these words, where a
    /// delivery's own envelope update is what draws a delivery, and routing
    /// one of those through here would draw its words twice.
    pub fn dispatch_forged_prompt(
        self: &Arc<Self>,
        key: &SessionSlot,
        text: String,
    ) -> Result<(), DispatchError> {
        let uuid = forge_sdk::request_id::next_prompt_id();
        let frame = Message::display_only_user(text.clone(), uuid.clone());
        let result = self.dispatch_workspace_prompt_under(
            key,
            text,
            crate::protocol::PromptSource::Forge,
            uuid,
        );
        if result.is_ok() {
            let _ = self.update_sender().send(SessionUpdate::ChatAppended {
                key: key.clone(),
                msg: frame,
                origin: None,
            });
        }
        result
    }

    /// Route a [`Command`]. Per-session commands (`cmd.key() ==
    /// Some(key)`) fan out to the matching `SessionTask`. App-level
    /// commands (`cmd.key() == None` - `SpawnProject`,
    /// `SpawnSession`, `StartDefault`) route to the workspace's own
    /// handler.
    ///
    /// Test fallback: when no `SessionTask` is registered for `key`
    /// but a `DomainSession` carries a stub `AgentHandle` (e.g., the
    /// `Workspace::testing_stub` path), the command runs
    /// synchronously against that handle. This keeps `#[test]`-flavor
    /// unit tests (no tokio runtime) able to observe the
    /// `forge_primitives::AgentCommand` emitted on the stub's channel
    /// without spinning up an async actor.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::UnknownSession`] when no `SessionTask`
    /// is registered for the requested key (e.g., the session was
    /// just closed), or [`DispatchError::SessionClosed`] when the
    /// task's command receiver has been dropped.
    pub fn dispatch(self: &Arc<Self>, cmd: Command) -> Result<(), DispatchError> {
        self.dispatch_with_origin(cmd, PromptOrigin::Ui)
    }

    /// Dispatch one command a view sent over the socket.
    ///
    /// Apart from the origin it stamps on a prompt's frame, this is
    /// [`Self::dispatch`] exactly. The two entries exist so the origin comes
    /// from the code path rather than from the command: a client cannot claim
    /// to be the terminal's own UI, which would have the terminal skip words
    /// it never saw.
    ///
    /// # Errors
    ///
    /// As [`Self::dispatch`].
    pub fn dispatch_from_view(self: &Arc<Self>, cmd: Command) -> Result<(), DispatchError> {
        self.dispatch_with_origin(cmd, PromptOrigin::View)
    }

    /// Route a command, emitting the user turn a prompt draws.
    ///
    /// A prompt is the reader's own words, and nothing else carries them: the
    /// CLI queues a prompt handed to it and never echoes it back, so a view
    /// drawing only frames shows the assistant answering something nobody saw.
    ///
    /// NOT from `dispatch_workspace_prompt`, which is the delivery path: a
    /// delivery already draws as an envelope turn of its own, so a bare user
    /// turn beside it would draw the same words twice.
    fn dispatch_with_origin(
        self: &Arc<Self>,
        cmd: Command,
        origin: PromptOrigin,
    ) -> Result<(), DispatchError> {
        // The id is minted HERE, before the prose is taken, so the prompt, its
        // lifecycle frames and the user turn below all carry one thing by id:
        // a plain `Prompt` becomes the `PromptUnder` the rest of the tree
        // already treats identically, keeping the mint on one path instead of
        // two.
        let cmd = match cmd {
            Command::Prompt { key, text, attachments } => Command::PromptUnder {
                key,
                text,
                attachments,
                uuid: forge_sdk::request_id::next_prompt_id(),
                source: crate::protocol::PromptSource::You,
            },
            other => other,
        };
        // The prose is taken before the move and emitted only once the command
        // reached a session. A refused prompt never reached a model, so no view
        // draws it as a turn - and the frame for one would open a live turn in
        // every view but the sender, with nothing left to close it: the refusal
        // is written to the asking socket alone.
        let prompt = match &cmd {
            Command::PromptUnder { key, text, uuid, .. } => {
                Some((key.clone(), text.clone(), uuid.clone()))
            }
            _ => None,
        };
        // A composer's text that names a forge command is FORGE'S, not the
        // CLI's. Decided here rather than in a view, because this is where
        // both of them dispatch: the terminal and a client reach the same
        // commands only by taking the same path, and some of these names the
        // CLI answers differently or not at all - `/new` is its own `/clear`,
        // which rotates a conversation forge never records.
        //
        // A forge command's answer comes back with the outcome rather than
        // being emitted where it is decided: it has to land after the echo
        // below, because a client draws this stream in arrival order and one
        // sent first drew the answer above the prompt it answers.
        let (outcome, answer) = match &cmd {
            Command::PromptUnder { key, text, .. } => match crate::prompt::forge_invocation(text) {
                Some(crate::prompt::Invocation::Command(prompt)) => {
                    self.run_forge_prompt(key, &prompt)
                }
                Some(crate::prompt::Invocation::Misuse(usage)) => {
                    (Ok(()), Some(forge_misuse(usage)))
                }
                None => match self.unrunnable_slash_name(key, text) {
                    Some(refusal) => (Ok(()), Some(forge_misuse(&refusal))),
                    None => (self.route(cmd), None),
                },
            },
            _ => (self.route(cmd), None),
        };
        if outcome.is_ok()
            && let Some((key, text, uuid)) = prompt
        {
            let _ = self.update_sender().send(SessionUpdate::ChatAppended {
                key: key.clone(),
                msg: Message::display_only_user(text, uuid),
                origin: Some(origin),
            });
            if let Some((severity, answer)) = answer {
                self.notice(&key, severity, &answer);
            }
        }
        outcome
    }

    /// Run one of forge's own commands against `key`'s seat.
    ///
    /// The launch settings are built here rather than taken from the caller:
    /// a view that supplied its own would spawn a session with what its own
    /// snapshot happened to hold, and a client has no snapshot to supply.
    ///
    /// The answer is returned rather than emitted, so the dispatcher can land
    /// it after the echo of the reader's own words.
    fn run_forge_prompt(
        self: &Arc<Self>,
        key: &SessionSlot,
        prompt: &crate::prompt::ForgePrompt,
    ) -> (Result<(), DispatchError>, Option<ForgeAnswer>) {
        use crate::prompt::ForgePrompt;
        // A mode the CLI has no name for is answered rather than sent: the
        // command carries the enum, so an unparsed one cannot be dispatched
        // at all.
        let command = match prompt {
            ForgePrompt::NewSession => {
                let (cwd, launch_settings) = self.spawn_inputs(key);
                Command::NewSession { key: key.clone(), cwd, launch_settings }
            }
            ForgePrompt::ResumeSession { session_id } => {
                let (cwd, launch_settings) = self.spawn_inputs(key);
                Command::ResumeSession {
                    key: key.clone(),
                    session_id: session_id.clone(),
                    cwd,
                    launch_settings,
                }
            }
            ForgePrompt::SetMode { mode } => {
                let Some(mode) = forge_primitives::permission::PermissionMode::from_wire(mode)
                else {
                    return (Ok(()), Some(forge_misuse(&format!("Unknown mode: {mode}"))));
                };
                Command::SetMode { key: key.clone(), mode }
            }
            ForgePrompt::SetModel { model } => {
                Command::SetModel { key: key.clone(), model: model.clone() }
            }
            ForgePrompt::SetEffort { level } => return self.set_effort(key, level),
        };
        (self.route(command), None)
    }

    /// The cwd and settings a re-spawn on `key` carries.
    fn spawn_inputs(&self, key: &SessionSlot) -> (String, SessionLaunchSettings) {
        let cwd = self.cwd_for_session(key).unwrap_or_default();
        let launch_settings = self.launch_settings_for(key, &cwd);
        (cwd, launch_settings)
    }

    /// Fill in the launch settings a caller did not build.
    ///
    /// The terminal builds them from its own snapshot of the documents and
    /// hands them over; a client has no config to read, so a spawn it asks
    /// for arrives with none, and a spawn that ran on those alone would carry
    /// no language, no model and the CLI's own defaults for permissions,
    /// effort and output style - a session launched differently from the one
    /// the same click starts in the terminal.
    ///
    /// Read from the workspace's own config dir, which is what a session with
    /// no account-scoped dir of its own runs under. The terminal builds its
    /// own from the account it is bound to, so the two can differ where an
    /// account names a dir of its own - and where they do, the caller that
    /// supplied settings keeps them.
    fn fill_launch_settings(&self, launch_settings: &mut SessionLaunchSettings, cwd: Option<&str>) {
        if launch_settings.settings.is_some() {
            return;
        }
        let empty = || serde_json::Value::Object(serde_json::Map::new());
        let documents_cwd = cwd.filter(|cwd| !cwd.is_empty()).map(std::path::Path::new);
        let documents =
            forge_agent::userdata::settings::settings_documents(self.config_dir(), documents_cwd);
        let user = documents.user.unwrap_or_else(empty);
        let local = documents.project_local.unwrap_or_else(empty);
        let preferences = self.user_preferences().unwrap_or_else(empty);
        let built = crate::launch_settings::session_launch_settings(
            &crate::launch_settings::LaunchSettingsDocuments {
                user: &user,
                local: &local,
                preferences: &preferences,
            },
        );
        launch_settings.language = built.language;
        launch_settings.settings = built.settings;
        launch_settings.agent_progress_summaries = built.agent_progress_summaries;
    }

    /// `/effort <level>`: write the level the next launch reads.
    ///
    /// A settings write rather than a session command - the CLI carries no
    /// control request for effort - so it lands in the same document the
    /// launch builder reads and takes effect when the session next starts.
    fn set_effort(
        self: &Arc<Self>,
        key: &SessionSlot,
        level: &str,
    ) -> (Result<(), DispatchError>, Option<ForgeAnswer>) {
        let Some(level) = forge_primitives::EffortLevel::from_stored(level) else {
            return (Ok(()), Some(forge_misuse(&format!("Unknown effort level: {level}"))));
        };
        // The seat's own config dir where there is one, and the one forge runs
        // under otherwise: effort is a user-level setting, so it does not
        // depend on a session being live.
        let config_dir =
            self.config_dir_for(key).unwrap_or_else(|| self.config_dir().to_path_buf());
        let path = config_dir.join("settings.json");
        let held = forge_agent::userdata::settings::settings_documents(&config_dir, None)
            .user
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        // A settings file holding something other than an object is one this
        // cannot add a key to. Writing the key alone is better than reporting
        // a success over a document nothing was added to.
        let mut document =
            if held.is_object() { held } else { serde_json::Value::Object(serde_json::Map::new()) };
        document["effortLevel"] = serde_json::Value::String(level.as_stored().to_owned());
        match forge_agent::userdata::settings::save_document(&path, &document) {
            Ok(()) => (
                Ok(()),
                Some((
                    NoticeSeverity::Info,
                    format!("Effort: {} (takes effect next session)", level.label()),
                )),
            ),
            Err(err) => (Ok(()), Some(forge_misuse(&format!("Failed to save effort: {err}")))),
        }
    }

    /// The line to answer a slash name this session cannot run, or `None`
    /// when the text is the CLI's to read.
    ///
    /// What a session can run is what its CLI advertised, or one of the names
    /// the CLI resolves without advertising ([`crate::prompt::FORWARDED`]).
    /// A session that has advertised nothing refuses nothing: an empty
    /// catalogue is not knowing, and refusing on ignorance would drop names
    /// the CLI has.
    fn unrunnable_slash_name(&self, key: &SessionSlot, text: &str) -> Option<String> {
        let name = text.split_whitespace().next()?;
        if !name.starts_with('/') {
            return None;
        }
        let advertised = self.available_commands_for(key);
        if advertised.is_empty()
            || advertised
                .iter()
                .any(|command| forge_agent::translate::commands::slash_name(&command.name) == name)
            || crate::prompt::is_forwarded_name(name)
        {
            return None;
        }
        Some(format!("{name} is not yet supported"))
    }

    /// Emit one line the core has for a view about `key`.
    fn notice(&self, key: &SessionSlot, severity: NoticeSeverity, text: &str) {
        let _ = self.update_sender().send(SessionUpdate::Notice {
            key: key.clone(),
            severity,
            text: text.to_owned(),
        });
    }

    /// The settings a launch on `key` carries, read from the same documents
    /// the CLI reads.
    fn launch_settings_for(&self, key: &SessionSlot, cwd: &str) -> SessionLaunchSettings {
        let empty = || serde_json::Value::Object(serde_json::Map::new());
        // An empty cwd is no cwd: joining `.claude/settings.local.json` onto
        // one would read it against the process working directory.
        let documents_cwd = (!cwd.is_empty()).then(|| std::path::Path::new(cwd));
        let documents = self.settings_documents(key, documents_cwd);
        let user =
            documents.as_ref().and_then(|documents| documents.user.clone()).unwrap_or_else(empty);
        let local = documents
            .as_ref()
            .and_then(|documents| documents.project_local.clone())
            .unwrap_or_else(empty);
        let preferences = self.user_preferences().unwrap_or_else(empty);
        crate::launch_settings::session_launch_settings(
            &crate::launch_settings::LaunchSettingsDocuments {
                user: &user,
                local: &local,
                preferences: &preferences,
            },
        )
    }

    /// Route a command that carries no frame of its own.
    fn route(self: &Arc<Self>, mut cmd: Command) -> Result<(), DispatchError> {
        // A spawn a caller could not complete is completed here, ahead of
        // routing: a client has no config to read, so what it sends carries no
        // settings, and a spawn that ran on those alone would carry no
        // language and the CLI's own defaults for permissions, effort and
        // output style. Ahead of the intercept below because a command is
        // finished before it is dispatched, and what a test reads there is
        // what the handler will receive.
        if let Command::SpawnProject { project_name, launch_settings } = &mut cmd {
            let cwd = self
                .find_project_view_by_name(project_name)
                .map(|project| project.path.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.fill_launch_settings(launch_settings, Some(&cwd));
        }
        // Test intercept (when armed): capture EVERY Command - both
        // app-level and per-session - before any routing. Tests use
        // this to assert what would have been dispatched without
        // spinning up real subprocesses or stub SessionTasks. Always
        // a no-op in production builds without the testing feature.
        #[cfg(any(test, feature = "testing"))]
        {
            let mut intercept = self.command_intercept.lock();
            if let Some(buffer) = intercept.as_mut() {
                buffer.push(cmd);
                return Ok(());
            }
        }
        // A prompt already answered, or one that asked something else, is a
        // dock that is gone: the reader's click did nothing. The session task
        // itself just logs that, and by the time it sees the answer the caller
        // is long gone - so it is refused here, the last layer that can still
        // tell one.
        match &cmd {
            Command::RespondPermission { key, tool_id, .. } => {
                let waiting = self
                    .domain_session_for(key)
                    .is_some_and(|domain| domain.lock().awaits_permission(tool_id));
                if !waiting {
                    return Err(DispatchError::NoPromptWaiting {
                        key: key.clone(),
                        tool_id: tool_id.clone(),
                    });
                }
            }
            Command::RespondQuestion { key, tool_id, .. } => {
                let waiting = self
                    .domain_session_for(key)
                    .is_some_and(|domain| domain.lock().awaits_question(tool_id));
                if !waiting {
                    return Err(DispatchError::NoPromptWaiting {
                        key: key.clone(),
                        tool_id: tool_id.clone(),
                    });
                }
            }
            Command::RespondSlackPost { key, id, .. } => {
                let waiting = self.slack_draft_waiting(*id, key);
                if !waiting {
                    return Err(DispatchError::NoDraftWaiting { key: key.clone(), id: *id });
                }
            }
            Command::RespondBrowserHandOff { key, id, .. } => {
                let waiting = self.browser_handoff_waiting(*id, key);
                if !waiting {
                    return Err(DispatchError::NoBrowserHandOffWaiting {
                        key: key.clone(),
                        id: *id,
                    });
                }
            }
            _ => {}
        }
        if let Some(key) = cmd.key() {
            // The /dictate override edits are workspace state on the
            // DomainSession, never agent traffic: apply inline and
            // echo, ahead of the SessionTask routing below.
            match cmd {
                Command::SetDictateOverride { key, update } => {
                    return self.apply_dictate_override(&key, update);
                }
                Command::ResetDictateOverrides { key } => {
                    return self.apply_dictate_override(
                        &key,
                        crate::dictate::DictateOverrideUpdate::Reset,
                    );
                }
                Command::SetDictateDevice { key, pick } => {
                    return self.apply_dictate_device(&key, pick);
                }
                _ => {}
            }
            let key = key.clone();
            // /new and /resume re-spawn on the already-pooled handle, where
            // get_agent_handle_at_key's stamp never runs.
            if let Command::NewSession { launch_settings, .. }
            | Command::ResumeSession { launch_settings, .. } = &mut cmd
            {
                let pooled = self
                    .pool
                    .lock()
                    .get(&key)
                    .map(|p| (p.permission_mode, p.registration.clone(), p.account.clone()));
                match pooled {
                    None => tracing::warn!(
                        target: "forge_workspace::workspace",
                        slot = %key.display(),
                        "permission_mode stamp skipped: respawn routed but no pool entry \
                         (release_session teardown window)",
                    ),
                    Some((mode, registration, account)) => {
                        // The respawned CLI runs the project's model too:
                        // this path bypasses the spawn-time stamp.
                        let project = registration.as_ref().and_then(|registration| {
                            self.config
                                .projects
                                .iter()
                                .find(|project| project.name == registration.project)
                        });
                        apply_project_model(project, launch_settings);
                        if let Some(mode) = mode {
                            spawn::stamp_permission_mode(launch_settings, mode);
                        } else {
                            tracing::debug!(
                                target: "forge_workspace::workspace",
                                slot = %key.display(),
                                account = %account.0,
                                "respawn keeps the launcher default: no project resolved for \
                                 this session, so there is no permission mode to stamp",
                            );
                        }
                    }
                }
            }
            let senders = self.command_senders.lock();
            if let Some(sender) = senders.get(&key) {
                // Stamp turn_pending only on the routed path (set + route
                // together) so the in-flight guards can't race a Prompt
                // whose wire-lagged `Running` echo hasn't landed yet. The
                // committed turn also spends the previous turn's failure
                // mark: the newest turn is this one now.
                if matches!(cmd, Command::Prompt { .. } | Command::PromptUnder { .. })
                    && let Some(domain) = self.domain_session_for(&key)
                {
                    let mut guard = domain.lock();
                    // A prompt committed with no turn in flight starts a new
                    // one, and a cancel stamp the last turn left armed expires
                    // here: nothing would spend it, and left standing it would
                    // exempt this turn's genuine failure. A prompt that only
                    // queues behind a busy turn keeps the stamp - that turn
                    // is still the one it was armed for, and its error Result
                    // still has to read as the reader's own cancel.
                    //
                    // The nudge and the classification it was read against
                    // are the running turn's for the same reason: a queued
                    // prompt is not the turn that will fail next.
                    if !guard.turn_in_flight() {
                        guard.pending_cancel = false;
                        guard.auto_continue = None;
                        guard.last_api_retry = None;
                    }
                    guard.turn_pending = true;
                    guard.failed_turn_at = None;
                }
                // Arm the cancel stamp on the routed path, so the turn's
                // own failed `Result` can tell a reader's interrupt from a
                // genuine error. An idle Cancel arms nothing.
                if matches!(cmd, Command::Cancel { .. })
                    && let Some(domain) = self.domain_session_for(&key)
                {
                    let mut guard = domain.lock();
                    if guard.turn_in_flight() {
                        guard.pending_cancel = true;
                    }
                }
                return sender.send(cmd).map_err(|_| DispatchError::SessionClosed(key));
            }
            drop(senders);
            // In production this branch returns `UnknownSession`: a
            // per-session `Command` arrives before a `SessionTask`
            // exists, which is the contract violation the error
            // signals. Under `cfg(any(test, feature = "testing"))`,
            // tests that wire a stub `AgentHandle` directly onto a
            // `DomainSession` (without a running tokio runtime to host
            // a real `SessionTask`) get a synchronous fallback so the
            // command still reaches the stub. Gating this here means
            // the fallback is structurally unreachable in production -
            // a future refactor can't open the race window silently.
            #[cfg(any(test, feature = "testing"))]
            {
                let Some(handle) = self.agent_handle_for(&key) else {
                    return Err(DispatchError::UnknownSession(key));
                };
                let sid = self.domain_handles.lock().get(&key).and_then(|d| {
                    d.lock().session_id.as_ref().map(std::string::ToString::to_string)
                });
                let fresh = if matches!(cmd, Command::NewSession { .. }) {
                    Some(self.fresh_session_id_for(&key))
                } else {
                    None
                };
                // A respawn moves the id the child runs under, so its
                // gateway binding moves with it. The routed path stamps
                // in the `SessionTask`, where `/new`'s id is minted;
                // this fallback holds the same id here, so it stamps the
                // same way rather than leaving the child on the previous
                // occupant's segment.
                let respawn_id = match &cmd {
                    Command::NewSession { .. } => fresh.clone(),
                    Command::ResumeSession { session_id, .. } => Some(session_id.clone()),
                    _ => None,
                };
                if let Some(respawn_id) = respawn_id.as_deref()
                    && let Command::NewSession { launch_settings, .. }
                    | Command::ResumeSession { launch_settings, .. } = &mut cmd
                {
                    self.stamp_respawn_overrides(&key, respawn_id, launch_settings);
                }
                crate::session_task::execute_command_via_handle(
                    &handle,
                    &key,
                    sid.as_deref(),
                    fresh,
                    cmd,
                )
                .map_err(|_| DispatchError::SessionClosed(key))
            }
            #[cfg(not(any(test, feature = "testing")))]
            {
                let _ = cmd;
                Err(DispatchError::UnknownSession(key))
            }
        } else {
            // App-level commands. The `spawn::*` handlers are sync -
            // they emit one event, kick off `get_agent_handle_at_key`
            // (which internally tokio::spawns the agent), and return.
            // Run them inline under the span; no detach needed.
            match cmd {
                Command::SpawnProject { project_name, launch_settings } => {
                    let span = tracing::info_span!(
                        "spawn_project",
                        project = %project_name,
                    );
                    let _enter = span.enter();
                    spawn::handle_spawn_project(self, &project_name, launch_settings);
                }
                Command::SpawnSession { key, role, launch_settings } => {
                    let span = tracing::info_span!(
                        "spawn_session",
                        slot = %key.display(),
                    );
                    let _enter = span.enter();
                    spawn::handle_spawn_session(self, &key, &role, launch_settings);
                }
                Command::StartDefault { project_name, launch_settings } => {
                    let span = tracing::info_span!(
                        "start_default",
                        project = ?project_name,
                    );
                    let _enter = span.enter();
                    spawn::handle_start_default(self, project_name, launch_settings);
                }
                Command::DeliverPeerPrompt { caller, target_project, wrapped } => {
                    let span = tracing::info_span!(
                        "deliver_peer_prompt",
                        target = %target_project,
                        message_id = %wrapped.id,
                    );
                    let _enter = span.enter();
                    spawn::handle_deliver_peer_prompt(self, &caller, target_project, wrapped);
                }
                Command::SpawnWorker {
                    project_key,
                    label,
                    charter,
                    spawned_by,
                    resume_existing,
                    kick,
                    resume_kick,
                    interactive,
                    mcp_families,
                    from_boot_respawn,
                    return_to,
                } => {
                    let span = tracing::info_span!(
                        "spawn_worker",
                        project = %project_key.as_str(),
                        label = %label,
                        resume = resume_existing.is_some(),
                        interactive,
                    );
                    let _enter = span.enter();
                    spawn::handle_spawn_worker(
                        self,
                        project_key,
                        spawn::WorkerSpawnArgs {
                            label,
                            charter,
                            kick,
                            resume_kick,
                            interactive,
                            mcp_families,
                        },
                        spawned_by,
                        resume_existing.as_deref(),
                        from_boot_respawn,
                        return_to.unwrap_or_else(crate::protocol::unanswerable),
                    );
                }
                Command::CloseWorker { project_key, label } => {
                    let span = tracing::info_span!(
                        "close_worker",
                        project = %project_key.as_str(),
                        label = %label,
                    );
                    let _enter = span.enter();
                    spawn::handle_close_worker(self, &project_key, &label);
                }
                Command::DespawnWorker { project_key, label, force, respond } => {
                    let span = tracing::info_span!(
                        "despawn_worker",
                        project = %project_key.as_str(),
                        label = %label,
                        force,
                    );
                    let _enter = span.enter();
                    spawn::handle_despawn_worker(
                        self,
                        &project_key,
                        &label,
                        force,
                        respond.unwrap_or_else(crate::protocol::unanswerable),
                    );
                }
                Command::DeliverWorkerPrompt { caller, project_key, target_label, wrapped } => {
                    let span = tracing::info_span!(
                        "deliver_worker_prompt",
                        project = %project_key.as_str(),
                        label = %target_label,
                        message_id = %wrapped.id,
                    );
                    let _enter = span.enter();
                    spawn::handle_deliver_worker_prompt(
                        self,
                        &caller,
                        &project_key,
                        &target_label,
                        wrapped,
                    );
                }
                Command::DeliverWorkerPromptToLead { caller, target_lead_key, wrapped } => {
                    let span = tracing::info_span!(
                        "deliver_worker_prompt_to_lead",
                        slot = %target_lead_key.display(),
                        message_id = %wrapped.id,
                    );
                    let _enter = span.enter();
                    spawn::handle_deliver_worker_prompt_to_lead(
                        self,
                        &caller,
                        &target_lead_key,
                        wrapped,
                    );
                }
                Command::DeliverGotifyMessage { project, team_role, notification } => {
                    let span = tracing::info_span!(
                        "deliver_gotify_message",
                        project = %project,
                        app = %notification.app,
                        priority = notification.priority,
                    );
                    let _enter = span.enter();
                    spawn::deliver_gotify_message(
                        self,
                        &project,
                        team_role.as_deref(),
                        notification,
                    );
                }
                Command::RespondSlackPost { key, id, approved } => {
                    // The guard above already refused a draft this one is not
                    // waiting for, so a `false` here is a resolve that landed
                    // between the two: the same refusal, not a silent drop.
                    let answered = self.resolve_slack_draft(
                        id,
                        &key,
                        forge_primitives::slack::SlackDraftEnding::Answered { approved },
                    );
                    if !answered {
                        return Err(DispatchError::NoDraftWaiting { key: key.clone(), id });
                    }
                }
                Command::RespondBrowserHandOff { key, id, done } => {
                    // Same shape as the Slack arm above: the guard already
                    // refused one that is not waiting, so a `false` here is a
                    // resolve that landed between the two.
                    // **Who answered is the log's business**: hand-offs were
                    // seen resolving by themselves in a live round
                    // (2026-10-07), and nothing named the hand that sent it.
                    tracing::debug!(
                        event_name = "browser_hand_off_answered",
                        key = ?key,
                        id = %id,
                        done,
                        "a client answered a parked browser hand-off"
                    );
                    let ending = if done {
                        forge_primitives::browser::HandOffEnding::Done
                    } else {
                        forge_primitives::browser::HandOffEnding::NotNow
                    };
                    let answered = self.resolve_browser_hand_off(id, &key, ending);
                    if !answered {
                        return Err(DispatchError::NoBrowserHandOffWaiting {
                            key: key.clone(),
                            id,
                        });
                    }
                }
                Command::OpenUrl { url } => {
                    let span = tracing::info_span!("open_url", url = %url);
                    let _enter = span.enter();
                    spawn::handle_open_url(self, url);
                }
                Command::DictateCatalogueCheck => {
                    return self.check_dictate_catalogue();
                }
                Command::DictateInstall { variant } => {
                    return self.start_install(variant);
                }
                Command::DictateActivate { role, file } => {
                    return self.start_activate(role, file);
                }
                Command::DictateDeactivate { role } => {
                    return self.start_deactivate(role);
                }
                Command::DictateBench { target, tier } => {
                    return self.start_bench(target, tier);
                }
                Command::DictateBenchStop => {
                    return self.stop_bench();
                }
                Command::DictateReadAloudStart { initiator } => {
                    let outcome = self.start_read_aloud(initiator);
                    self.push_models();
                    return outcome;
                }
                Command::DictateReadAloudStop { keep, initiator } => {
                    let outcome = self.finish_read_aloud(keep, initiator);
                    self.push_models();
                    return outcome;
                }
                Command::DictateReadAloudDelete { id } => {
                    let outcome = self.delete_read_aloud(&id);
                    self.push_models();
                    return outcome;
                }
                Command::DictateUninstall { file } => {
                    let outcome = self.uninstall_model(&file);
                    self.push_models();
                    return outcome;
                }
                Command::DictateBenchDelete { target, tier, corpus } => {
                    let outcome = self.delete_bench_result(&target, tier, &corpus);
                    self.push_models();
                    return outcome;
                }
                Command::DictateStart { key } => {
                    let ws = Arc::clone(self);
                    tokio::spawn(async move {
                        crate::dictate::handle_dictate_start(&ws, key).await;
                    });
                }
                Command::DictateStream { key, options, initiator } => {
                    let updates = self.update_sender();
                    // Registered HERE, inline, rather than on a spawned
                    // task: the connection this arrived on is ordered, so
                    // the audio frame a client sends next must find the
                    // take. Nothing on this path opens a device, so there
                    // is nothing to take off the runtime thread. The test
                    // `a_stream_take_registers_synchronously_and_keeps_its_frames`
                    // fails the moment this moves onto one.
                    match crate::dictate::register_stream_take(self, &key, options, initiator) {
                        Ok(take) => {
                            let _ = updates.send(SessionUpdate::DictateStarted {
                                key: key.clone(),
                                floor_db: take.floor_db,
                                generation: take.generation,
                                initiator,
                            });
                            tokio::spawn(crate::dictate::run_stream_take(
                                Arc::clone(self),
                                key,
                                take,
                                updates,
                            ));
                        }
                        Err(message) => {
                            // The start refused, so any park the press's
                            // release left behind answers a take that will
                            // never exist.
                            self.dictate_runtime.lock().clear_stop_pending(&key, initiator);
                            let _ = updates.send(SessionUpdate::DictateEnded {
                                key,
                                outcome: crate::protocol::DictateOutcome::Refused { message },
                                generation: 0,
                                initiator,
                            });
                        }
                    }
                }
                Command::DictateStop { key, submit, initiator } => {
                    let ws = Arc::clone(self);
                    tokio::spawn(async move {
                        crate::dictate::handle_dictate_stop(&ws, &key, submit, initiator).await;
                    });
                }
                // User-action store writes routed through the command
                // bus. Synchronous inline handlers - the writes are
                // local redb operations, and the TUI has already
                // applied its optimistic state.
                Command::SaveReviewThreads { project, branch, threads } => {
                    let span = tracing::info_span!(
                        "save_review_threads",
                        project = %project,
                        branch = %branch,
                    );
                    let _enter = span.enter();
                    self.save_review_threads(&project, &branch, &threads);
                }
                Command::RemoveReviewThread { project, branch, thread_id } => {
                    let span = tracing::info_span!(
                        "remove_review_thread",
                        project = %project,
                        branch = %branch,
                        thread_id = %thread_id,
                    );
                    let _enter = span.enter();
                    self.remove_review_thread(&project, &branch, &thread_id);
                }
                Command::SetReviewThreadStatus { project, branch, thread_id, status } => {
                    let span = tracing::info_span!(
                        "set_review_thread_status",
                        project = %project,
                        branch = %branch,
                        thread_id = %thread_id,
                        status = ?status,
                    );
                    let _enter = span.enter();
                    self.set_review_thread_status(&project, &branch, &thread_id, status);
                }
                Command::CloseSession { session_key } => {
                    let span = tracing::info_span!(
                        "close_session",
                        slot = %session_key.display(),
                    );
                    let _enter = span.enter();
                    self.release_session_with_cascade(&session_key);
                }
                Command::UpsertReviewThread { project, branch, thread, respond } => {
                    let span = tracing::info_span!(
                        "upsert_review_thread",
                        project = %project,
                        branch = %branch,
                        thread_id = %thread.id,
                    );
                    let _enter = span.enter();
                    if let Some(respond) = respond {
                        let _ = respond.send(self.upsert_review_thread(&project, &branch, thread));
                    }
                }
                Command::SubmitReview { project, branch, summary, thread_ids, origin, respond } => {
                    let span = tracing::info_span!(
                        "submit_review",
                        project = %project,
                        branch = %branch,
                        threads = thread_ids.len(),
                    );
                    let _enter = span.enter();
                    if let Some(respond) = respond {
                        let _ = respond.send(self.submit_review(
                            &project,
                            &branch,
                            summary,
                            &thread_ids,
                            origin,
                        ));
                    }
                }
                // The board's edits are the user's own moves: stamped
                // `By::User` in the history, and no session routes them -
                // the board is the user's surface, and each carries its
                // project.
                Command::TaskVerdict { project, id, approve, words } => {
                    let id = TaskId::from(id.as_str());
                    let moved = if approve {
                        self.approve_task(&project, &id)
                    } else {
                        self.send_back_task(&project, &id, words.as_deref().unwrap_or(""))
                    };
                    match moved {
                        Ok(Some(task)) => {
                            // A send-back is news its owner wants: one
                            // message, down the same ladder the chase
                            // uses - a live owner hears it, a sleeping one
                            // reads the row.
                            if !approve
                                && let Some(owner) = &task.owner
                                && self.seat_is_live(owner)
                            {
                                let text = format!(
                                    "task board: \"{}\" came back from the user's look - the \
                                     words are in its detail; resume it.",
                                    task.subject,
                                );
                                let _ = self.dispatch_workspace_prompt_from(
                                    owner,
                                    text,
                                    PromptSource::Forge,
                                );
                            }
                        }
                        Ok(None) => {
                            tracing::debug!(
                                target: "forge_workspace",
                                project = %project,
                                id = %id.as_str(),
                                "a user verdict named a row that is not there",
                            );
                            self.board_edit_refused(
                                "a verdict",
                                "the row it named is no longer there",
                            );
                        }
                        Err(refused) => {
                            tracing::debug!(
                                target: "forge_workspace",
                                project = %project,
                                id = %id.as_str(),
                                refusal = %refused,
                                "a user verdict was refused",
                            );
                            self.board_edit_refused("a verdict", &refused.to_string());
                        }
                    }
                }
                Command::TaskAnswer { project, id, words } => {
                    let id = TaskId::from(id.as_str());
                    match self.answer_task(&project, &id, &words) {
                        Ok(Some(task)) => {
                            if let Some(owner) = &task.owner
                                && self.seat_is_live(owner)
                            {
                                let text = format!(
                                    "task board: the user answered \"{}\" - the words are in its \
                                     detail; resume it.",
                                    task.subject,
                                );
                                let _ = self.dispatch_workspace_prompt_from(
                                    owner,
                                    text,
                                    PromptSource::Forge,
                                );
                            }
                        }
                        Ok(None) => {
                            tracing::debug!(
                                target: "forge_workspace",
                                project = %project,
                                id = %id.as_str(),
                                "a user answer named a row that is not there",
                            );
                            self.board_edit_refused(
                                "an answer",
                                "the row it named is no longer there",
                            );
                        }
                        Err(refused) => {
                            tracing::debug!(
                                target: "forge_workspace",
                                project = %project,
                                id = %id.as_str(),
                                refusal = %refused,
                                "a user answer was refused",
                            );
                            self.board_edit_refused("an answer", &refused.to_string());
                        }
                    }
                }
                Command::TaskRank { project, id, to } => {
                    match self.rank_task(&project, &TaskId::from(id.as_str()), to) {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            tracing::debug!(
                                target: "forge_workspace::tasks",
                                project = %project,
                                id = %id.as_str(),
                                "a board re-order named a row that is not there",
                            );
                            self.board_edit_refused(
                                "a re-order",
                                "the row it named is no longer there",
                            );
                        }
                        Err(refused) => {
                            tracing::debug!(
                                target: "forge_workspace::tasks",
                                project = %project,
                                id = %id.as_str(),
                                refusal = %refused,
                                "a board re-order was refused",
                            );
                            self.board_edit_refused("a re-order", &refused.to_string());
                        }
                    }
                }
                Command::TaskMove { project, id, to } => {
                    match self.user_move_task(&project, &TaskId::from(id.as_str()), to) {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            tracing::debug!(
                                target: "forge_workspace::tasks",
                                project = %project,
                                id = %id.as_str(),
                                "a board move named a row that is not there",
                            );
                            self.board_edit_refused(
                                "a move",
                                "the row it named is no longer there",
                            );
                        }
                        Err(refused) => {
                            tracing::debug!(
                                target: "forge_workspace::tasks",
                                project = %project,
                                id = %id.as_str(),
                                refusal = %refused,
                                "a board move was refused",
                            );
                            self.board_edit_refused("a move", &refused.to_string());
                        }
                    }
                }
                Command::TaskAssign { project, id, owner } => {
                    // A project this forge does not carry has no seat to name
                    // and no row to hold: an assignment to it is refused
                    // rather than written against an invented empty-org slot.
                    if let Some(row) = self.config.projects.iter().find(|p| p.name == project) {
                        let owner_slot =
                            owner.map(|label| SessionSlot::new(&row.org, &project, label.clone()));
                        match self.assign_task(&project, &TaskId::from(id.as_str()), owner_slot) {
                            Ok(Some(_)) => {}
                            Ok(None) => {
                                tracing::debug!(
                                    target: "forge_workspace::tasks",
                                    project = %project,
                                    id = %id.as_str(),
                                    "a board assignment named a row that is not there",
                                );
                                self.board_edit_refused(
                                    "an assignment",
                                    "the row it named is no longer there",
                                );
                            }
                            Err(refused) => {
                                tracing::debug!(
                                    target: "forge_workspace::tasks",
                                    project = %project,
                                    id = %id.as_str(),
                                    refusal = %refused,
                                    "a board assignment was refused",
                                );
                                self.board_edit_refused("an assignment", &refused.to_string());
                            }
                        }
                    } else {
                        self.board_edit_refused("an assignment", "no project carries that name");
                    }
                }
                Command::TaskCreate { project, subject, parent } => {
                    let now = std::time::SystemTime::now();
                    let task = forge_primitives::tasks::Task {
                        id: TaskId::from(uuid::Uuid::new_v4().to_string()),
                        project_name: project.clone(),
                        subject,
                        active_form: None,
                        detail: None,
                        status: forge_primitives::tasks::TaskStatus::Pending,
                        owner: None,
                        parent: parent.map(|id| TaskId::from(id.as_str())),
                        waiting_on: None,
                        estimate: None,
                        rank: None,
                        verify: None,
                        links: Vec::new(),
                        attempt: 0,
                        archived_at: None,
                        created_at: now,
                        updated_at: now,
                    };
                    self.push_task(task);
                }
                other => {
                    tracing::warn!(
                        target: "forge_workspace",
                        command = ?other,
                        "unexpected App-level command (no key but not a spawn variant); ignored",
                    );
                }
            }
            Ok(())
        }
    }

    /// Say on the service line why a board edit did not land: a refusal a
    /// reader cannot see is indistinguishable from a press that did
    /// nothing.
    fn board_edit_refused(&self, what: &str, why: &str) {
        let _ = self.update_tx.send(SessionUpdate::ServiceStatus {
            severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
            message: format!("The board refused {what}: {why}"),
        });
    }

    /// Re-spawn this project's persisted workers on lead reconnect,
    /// resuming by label off the id the store holds for that label.
    /// Charter and kick come from the DB row.
    ///
    /// Since this holds the worker set, it decides the kick directly:
    /// a resume takes the row's own `resume_kick` and falls back to the
    /// forge restart note, telling it to continue rather than restart; a
    /// fresh re-spawn (never prompted, so no store row) re-delivers the
    /// stored kick. `maybe_kick_worker_on_connected` then just delivers
    /// whatever this put on the `WorkerEntry`.
    pub(crate) fn dispatch_worker_respawns(
        self: &Arc<Self>,
        lead: &SessionSlot,
        project_key: &crate::target::ProjectKey,
        dynamic: &[crate::store::sessions::SessionRecord],
        force_new: bool,
    ) {
        // `--new`: the lead came up fresh, so its workers do too - every
        // row spawns fresh rather than resuming its stored id.
        let project = if force_new { None } else { self.project_for_key(project_key) };
        for worker in dynamic {
            let resume_existing = match project
                .as_ref()
                .map(|p| self.stored_session_id(&p.org, &p.name, &worker.label))
            {
                None | Some(Ok(None)) => None,
                Some(Ok(Some(id))) => Some(id),
                Some(Err(error)) => {
                    // A row that cannot be read is not a row without an
                    // id: spawning fresh here would mint over it and fork
                    // the worker's session.
                    tracing::error!(
                        target: "forge_workspace::workers",
                        project = %project_key.as_str(),
                        label = %worker.label,
                        %error,
                        "reading the worker's session row failed; skipping this worker rather than spawning fresh over it",
                    );
                    continue;
                }
            };
            // A resume starts the subprocess in the worker's worktree,
            // and a subprocess cannot enter a directory that is not
            // there: the spawn fails on every boot and the row never
            // clears. A FRESH re-spawn runs in the project root and
            // takes `--worktree`, so it is the resume this skip is for:
            // a fresh one that cannot get a worktree is refused by the
            // spawn itself, which leaves its row failed with the reason.
            if let Some(root) = project.as_ref().map(|p| p.path.as_path())
                && !crate::mcp::workers::types::worker_row_can_start(
                    root,
                    &worker.label,
                    worker.is_git_repo,
                    resume_existing.is_some(),
                )
            {
                let wanted = crate::mcp::workers::types::worker_tag_dir(
                    root,
                    &worker.label,
                    matches!(worker.is_git_repo, Some(true)),
                );
                tracing::warn!(
                    target: "forge_workspace::workers",
                    event_name = "boot_respawn_skipped_missing_worktree",
                    project = %project_key.as_str(),
                    label = %worker.label,
                    directory = %wanted.display(),
                    "the worker's directory is gone, so this boot re-spawn has nowhere to \
                     start; the row is kept and stays unoffered until it is back",
                );
                continue;
            }
            let kick = if resume_existing.is_some() {
                worker.resume_kick.clone().or_else(|| Some(DYNAMIC_WORKER_RESTART_NOTE.to_owned()))
            } else {
                worker.kick.clone()
            };
            let (tx, _rx) = tokio::sync::oneshot::channel();
            let cmd = crate::protocol::Command::SpawnWorker {
                project_key: project_key.clone(),
                label: worker.label.clone(),
                charter: worker.charter.clone().unwrap_or_default(),
                spawned_by: lead.clone(),
                resume_existing,
                kick,
                // The row this re-spawn is replaying already holds it.
                resume_kick: None,
                // And its family selection rides the same row; the spawn
                // reads it back rather than restating it here.
                mcp_families: worker.mcp_families.clone(),
                interactive: worker.interactive.unwrap_or(false),
                from_boot_respawn: true,
                return_to: Some(tx),
            };
            if let Err(err) = self.dispatch(cmd) {
                tracing::error!(
                    target: "forge_workspace::workers",
                    project = %project_key.as_str(),
                    label = %worker.label,
                    error = ?err,
                    "dispatch_worker_respawns: dispatch failed for label"
                );
            }
        }
    }

    /// Lead Connected-hook entry point. Synchronously claims a
    /// per-project in-flight guard, then dispatches one
    /// `Command::SpawnWorker` per persisted row, resuming by the id the
    /// store holds for that row's label. The guard is released after
    /// the dispatches go out so a fast double-Connected can't slip a
    /// second wave through.
    ///
    /// No-op when the per-project guard is already claimed (another
    /// wave is in flight). The first-pass `live_workers.is_empty()`
    /// gate in `session_task::maybe_respawn_workers_on_connected` catches
    /// the post-dispatch case; this guard covers the in-flight window.
    pub(crate) fn respawn_workers_for_lead(
        self: &Arc<Self>,
        lead: &SessionSlot,
        project_key: crate::target::ProjectKey,
        force_new: bool,
    ) {
        let dynamic = self.worker_rows_for_project(&project_key);
        if dynamic.is_empty() {
            return;
        }
        if !self.try_claim_respawn(&project_key) {
            tracing::debug!(
                target: "forge_workspace::workers",
                project = %project_key.as_str(),
                "worker-spawn already in flight; skipping duplicate Connected fire",
            );
            return;
        }
        // Dispatching a worker probes its project path for git-repo-ness
        // inline, so this runs off the event loop rather than inside
        // `translate_event`. Outside a runtime (the sync `#[test]`
        // fixtures in `connected_hook_tests`) there is none to hand it
        // to, so dispatch inline.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                target: "forge_workspace::workers",
                project = %project_key.as_str(),
                "no tokio runtime in scope; dispatching worker-spawns inline (test path)",
            );
            self.dispatch_worker_respawns(lead, &project_key, &dynamic, force_new);
            self.release_respawn(&project_key);
            return;
        };
        let workspace = Arc::clone(self);
        let lead = lead.clone();
        handle.spawn(async move {
            tracing::info!(
                target: "forge_workspace::workers",
                project = %project_key.as_str(),
                lead_slot = %lead.display(),
                worker_count = dynamic.len(),
                "dispatching SpawnWorker per persisted row",
            );
            workspace.dispatch_worker_respawns(&lead, &project_key, &dynamic, force_new);
            workspace.release_respawn(&project_key);
        });
    }

    /// Claim the per-project respawn in-flight guard. Returns true
    /// if the guard was acquired (entry was absent), false if another
    /// wave was already in flight.
    fn try_claim_respawn(&self, project_key: &crate::target::ProjectKey) -> bool {
        self.respawn_in_flight.lock().insert(project_key.clone())
    }

    /// Whether this fire pass is the FIRST to find `id` unwakeable,
    /// recording it as seen if so. True is the pass that warns; false is
    /// every later pass while the condition holds.
    pub(crate) fn cron_unwakeable_is_new(&self, id: &forge_primitives::CronId) -> bool {
        self.unwakeable_crons.lock().insert(id.clone())
    }

    /// Close a fire pass: `ids` is what it found unwakeable, and it
    /// replaces the previous pass's set wholesale. That replacement is
    /// the only clearing this state needs - an entry cannot survive a
    /// pass that did not find its cron unwakeable, whether the cron fired,
    /// advanced, was deleted, or simply was not due.
    pub(crate) fn cron_unwakeable_commit(
        &self,
        ids: std::collections::HashSet<forge_primitives::CronId>,
    ) {
        *self.unwakeable_crons.lock() = ids;
    }

    /// The session a worker label resumes onto.
    ///
    /// The row the label is registered under is authoritative while it is
    /// there: a worker that has one opens as it always has. The
    /// transcripts are the recovery path for a row that is gone - a
    /// despawn deleted it, or this install never had it - and there the
    /// newest transcript in `run_dir`'s directory carrying the label's
    /// worker tag wins.
    ///
    /// `Ok(ResumeTarget::None)` is a label with nothing in either, and
    /// `Err` is a store, directory or transcript that is there but could
    /// not be read. Only the first is a fallback: a caller that started a
    /// fresh session on the second would be answering a question the
    /// lookup never answered.
    pub(crate) fn resolve_worker_resume_session(
        &self,
        org: &str,
        project: &str,
        run_dir: &std::path::Path,
        label: &str,
    ) -> Result<ResumeTarget, anyhow::Error> {
        if let Some(session_id) = self.stored_session_id(org, project, label)? {
            return Ok(ResumeTarget::Found(session_id));
        }
        // The tag scan is what the store's cache is for: without it every
        // candidate transcript is read end to end, and the directory a
        // worker without a worktree runs in holds every session that ever
        // ran in its project. A cache that cannot be read or written
        // costs a re-scan, never an answer.
        let cache = {
            let db = self.db.lock();
            load_session_tag_cache(db.as_ref())
        };
        let found = forge_agent::userdata::transcripts::newest_worker_session(
            &self.config_dir,
            run_dir,
            label,
            Some(&cache),
        );
        {
            let db = self.db.lock();
            persist_session_tag_cache(db.as_ref(), &cache);
        }
        Ok(match found? {
            Some(session_id) => ResumeTarget::Found(session_id),
            None => ResumeTarget::None,
        })
    }

    /// Release the per-project respawn in-flight guard. Paired with
    /// `try_claim_respawn`; called once the dispatches have gone out, on
    /// each path that can issue them.
    fn release_respawn(&self, project_key: &crate::target::ProjectKey) {
        self.respawn_in_flight.lock().remove(project_key);
    }

    /// Set the `session_id` field on the workspace's `DomainSession`
    /// for `key`. No-op when no domain handle is registered for
    /// `key`. Used by `App::set_session_id` to stamp the
    /// claude-issued UUID once the first `Connected` event fires -
    /// the workspace consults this when routing `AgentHandle` calls
    /// that take a session_id.
    pub fn set_session_id_in_domain(
        &self,
        key: &SessionSlot,
        value: Option<forge_primitives::SessionId>,
    ) {
        let Some(domain) = self.domain_handles.lock().get(key).cloned() else {
            return;
        };
        domain.lock().session_id = value;
    }

    /// Graceful shutdown of every pooled Agent. Drains the pool, then
    /// drops each `Arc<AgentHandle>`.
    ///
    /// The subprocess kill is asynchronous through each `SessionTask`:
    /// dropping the command senders closes every task's command channel,
    /// each task's run loop exits, and its exit path awaits
    /// `AgentHandle::disconnect`, which takes the bridge's client slot
    /// and runs the SDK's graceful shutdown (signal reader task, drain,
    /// close the child). `Client` has no `Drop` of its own, so without
    /// that disconnect the child would survive the pool drain.
    /// forge-tui releases its handle reference before calling shutdown,
    /// so Workspace is the sole owner of every pool entry. Callers that
    /// hold cloned handles across shutdown keep the AgentHandle's task
    /// alive until they release them.
    pub fn shutdown(&self) {
        // Release any live dictation before the pools go: a recording
        // task outliving its session's teardown would otherwise hold
        // the microphone for a composer nobody can reach.
        crate::dictate::teardown_all(self);
        // Drop command senders first so every SessionTask sees its
        // command channel close and exits cleanly; each task's exit
        // path then disconnects its subprocess (see the doc above).
        let _ = self.command_senders.lock().drain().collect::<Vec<_>>();
        let _ = self.domain_handles.lock().drain().collect::<Vec<_>>();
        drop(self.pool.lock().drain().collect::<Vec<_>>());
    }

    /// Release a single session's pool entry: drops the workspace's
    /// `Arc<AgentHandle>` for that key so the underlying `claude`
    /// subprocess exits once the consumer (forge-tui's bucket) also
    /// drops its reference.
    ///
    /// Cascade-aware lead release. Use this when closing a project's
    /// lead session from the TUI: the lead-row `×` click, the launchpad's
    /// per-row close on a failed lead bucket, etc.
    ///
    /// When `session_key` is a project's lead - it appears in the
    /// project's catalog AND is NOT in `live_workers[project]` - every
    /// live worker under that project is released first via the
    /// non-cascading `release_session` primitive. Workers' JSONLs
    /// persist on disk; only the in-memory live state + the running
    /// claude subprocesses are torn down.
    ///
    /// The "in-catalog AND not-in-live_workers" rule is the
    /// discriminator (rather than `sessions.first()`): the catalog
    /// also indexes worker sessions once their Connected fires, so
    /// `sessions[0]` is not a reliable lead marker after a worker
    /// reaches Running. `live_workers` is the authoritative
    /// "this session is a child agent" registry.
    pub fn release_session_with_cascade(self: &Arc<Self>, session_key: &SessionSlot) {
        // Announced before anything is torn down, so the seat's row starts
        // saying where it is going at the gesture rather than after the
        // workers this cascade takes with it (#1930). A viewer's mark is a
        // set, so a second announcement would be spent.
        self.announce_release(session_key);
        // The cascade is the lead's, and a lead's slot is the one whose
        // label says so: a worker's slot carries its own label, so a
        // closed worker cannot be misidentified as its lead however far
        // the registry has moved.
        let cascade_project = session_key.is_lead().then(|| {
            self.list_projects()
                .into_iter()
                .find(|view| view.org == session_key.org() && view.name == session_key.project())
        });
        let cascade_project = cascade_project.flatten().map(|view| view.key);
        if let Some(project_key) = cascade_project {
            for entry in self.drain_live_workers(&project_key) {
                let worktree =
                    crate::protocol::WorktreeDisposition::untouched(entry.is_git_repo_at_spawn);
                let _ = self.update_tx.send(SessionUpdate::WorkerStatusChanged {
                    project_key: project_key.clone(),
                    action: crate::protocol::WorkerStatusAction::Removed,
                    status: entry.to_status(),
                    worktree,
                });
                self.release_session(&entry.slot);
            }
        }
        self.release_session(session_key);
    }

    /// Announce that a seat's release has begun.
    ///
    /// One place for the close doors to say it, so a viewer draws the seat as
    /// going to sleep from the gesture instead of keeping the state it last
    /// read. Each door calls it where the close is certain and the teardown
    /// has not started: a lead before its cascade, a worker before its own
    /// kill - where a despawn's removal frame only follows the worktree
    /// cleanup, minutes later (#1930).
    pub(crate) fn announce_release(self: &Arc<Self>, key: &SessionSlot) {
        let _ = self.update_tx.send(SessionUpdate::Releasing { key: key.clone() });
    }

    /// Non-cascading single-session release - the primitive. Drops
    /// the pool entry, command sender, and domain handle for
    /// `session_key`. No side effects beyond that single session.
    ///
    /// Use this for ANY session close where cascade semantics are
    /// undesirable or undefined:
    /// - `handle_close_worker` (per-row worker X click) - the close
    ///   must affect only the worker being closed.
    /// - The lead-cascade loop inside `release_session_with_cascade`
    ///   itself, when releasing each worker.
    ///
    /// Use `release_session_with_cascade` instead when the caller is
    /// the lead-row close gesture.
    pub(crate) fn release_session(self: &Arc<Self>, session_key: &SessionSlot) {
        crate::dictate::teardown_for_closed_session(self, session_key);
        // Anything parked for this session's owner was waiting on a
        // session that is now gone: fail the peer asks so their callers
        // are told, rather than letting them wait out the timeout.
        self.expire_parked_for_slot(
            session_key,
            crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
        );
        self.domain_handles.lock().remove(session_key);
        let removed = self.pool.lock().remove(session_key);
        drop(removed);
        let _ = self.command_senders.lock().remove(session_key);
    }

    /// Whether a session task still exists for `key`, which is what
    /// makes it live: its command channel is registered and routable.
    pub(crate) fn session_is_live(&self, key: &SessionSlot) -> bool {
        self.command_senders.lock().contains_key(key)
    }

    /// Supersession-safe release: drop `session_key`'s pool entry,
    /// command sender, and domain handle ONLY when the pooled agent is
    /// still `handle` (by `Arc` identity). A SessionTask whose session
    /// was re-spawned under the same key is a
    /// stale predecessor - when it exits and runs this cleanup, the
    /// pool already holds the successor's handle, so the guard no-ops
    /// and the live re-spawned session is left intact. Gating all three
    /// maps on the one identity check keeps a superseded task from
    /// half-cleaning its successor.
    pub(crate) fn release_session_if_current(
        &self,
        session_key: &SessionSlot,
        handle: &Arc<AgentHandle>,
    ) {
        let mut pool = self.pool.lock();
        if !pool.get(session_key).is_some_and(|pooled| Arc::ptr_eq(&pooled.handle, handle)) {
            return;
        }
        let removed = pool.remove(session_key);
        drop(pool);
        drop(removed);
        let _ = self.command_senders.lock().remove(session_key);
        let _ = self.domain_handles.lock().remove(session_key);
    }

    // ---- Live workers (project-internal child-agent coordination) ----

    /// Snapshot the live workers for `project_key`. Returns an empty
    /// Vec when no workers exist (rather than `None`) so the TUI tree-
    /// child render can branch only on `is_empty`.
    pub fn list_live_workers(
        &self,
        project_key: &ProjectKey,
    ) -> Vec<crate::mcp::workers::types::WorkerEntry> {
        self.live_workers.lock().get(project_key).cloned().unwrap_or_default()
    }

    /// Every live worker's liveness, keyed by project.
    ///
    /// The launchpad's worker rows read this once per frame, which is
    /// what the shape is for: one lock answers every project row, and
    /// the projection leaves each worker's charter in the registry
    /// instead of copying it, which [`Self::list_live_workers`] does on
    /// every call.
    pub fn live_worker_states_by_project(
        &self,
    ) -> HashMap<ProjectKey, Vec<crate::mcp::workers::types::LiveWorkerState>> {
        self.live_workers
            .lock()
            .iter()
            .map(|(project, workers)| {
                let states = workers
                    .iter()
                    .map(|w| crate::mcp::workers::types::LiveWorkerState {
                        label: w.label.clone(),
                        status: w.status,
                        slot: w.slot.clone(),
                        diagnostic: w.diagnostic.clone(),
                    })
                    .collect();
                (project.clone(), states)
            })
            .collect()
    }

    /// Whether `slot`'s session has live background work. A fact about the
    /// session rather than about a viewer, so the core holds one answer for
    /// every view.
    pub fn has_background_work(&self, slot: &SessionSlot) -> bool {
        self.domain_session_for(slot).is_some_and(|domain| domain.lock().background_work)
    }

    /// When `slot`'s newest turn ended in failure, for the rail's mark.
    /// `None` for a slot whose newest turn succeeded, never ran, was
    /// cancelled by the reader, or has since been moved past with a new
    /// turn.
    pub fn session_failed_turn(&self, slot: &SessionSlot) -> Option<std::time::SystemTime> {
        self.domain_session_for(slot).and_then(|domain| domain.lock().failed_turn_at)
    }

    /// What the session at `slot` is waiting on a person for, or `None`
    /// when it can advance on its own.
    pub fn pending_interaction(&self, slot: &SessionSlot) -> Option<PendingInteractionKind> {
        let domain = self.domain_session_for(slot)?;
        let guard = domain.lock();
        // A question outranks a permission prompt: both hold a turn, and
        // the question is the one a person has to read before answering.
        // A slot holding both therefore answers as the question.
        if guard.pending_interactions.values().any(|pending| {
            matches!(pending, crate::protocol::PendingInteractionSlot::Question { .. })
        }) {
            return Some(PendingInteractionKind::Question);
        }
        if guard.pending_interactions.values().any(|pending| {
            matches!(pending, crate::protocol::PendingInteractionSlot::Permission { .. })
        }) {
            return Some(PendingInteractionKind::Permission);
        }
        None
    }

    /// How many prompts the session at `slot` is holding, zero when none.
    /// The same set [`Self::pending_interaction`] answers the kind of, so a
    /// view showing one of them can say how many wait behind it.
    pub fn pending_interaction_depth(&self, slot: &SessionSlot) -> usize {
        self.domain_session_for(slot).map_or(0, |domain| domain.lock().pending_interactions.len())
    }

    /// Record a fatal error, so a view that was not attached when it fired
    /// can still read it.
    pub(crate) fn record_fatal_error(&self, error: forge_primitives::error::AppError) {
        *self.last_fatal_error.lock() = Some(error);
    }

    /// The last fatal error, or `None` when nothing has failed fatally.
    ///
    /// A fatal error is App-level: it names the startup that could not
    /// happen rather than a seat, and it arrives once. Without this the
    /// only way to know is to have been subscribed when it went out.
    pub fn last_fatal_error(&self) -> Option<forge_primitives::error::AppError> {
        self.last_fatal_error.lock().clone()
    }

    /// The statuspage's last answer, or `None`.
    ///
    /// `None` covers both "every relevant component is operational" and
    /// "the statuspage could not be reached", which is the fetch's own
    /// contract rather than something invented here - and a view that draws
    /// nothing for it draws what the terminal always drew.
    ///
    /// The core probes rather than the view because the answer is the same
    /// for every viewer and because it must outlive any one of them: a
    /// client built after the terminal is gone would otherwise never see a
    /// service status at all.
    pub fn service_status(&self) -> Option<ServiceIssue> {
        self.service_status.lock().clone()
    }

    /// Kick off the statuspage probe. Idempotent, and off the boot path.
    fn start_service_status_probe(&self, prober: ServiceStatusProber) {
        spawn_background_service_status_probe(
            &self.service_status_probe_started,
            &self.service_status,
            &self.update_tx,
            prober,
        );
    }

    /// What the seat at `slot` is holding - **a draft leads, then arrival
    /// order** - empty when it is holding nothing. Every hop keeps that
    /// order, so a re-read and the live folds cannot disagree about the
    /// front, with one exception: two drafts held at once come back in the
    /// registry's own order, because the registry keeps none to offer.
    ///
    /// **The list rather than one ask**, because a parallel batch parks
    /// several at once and a view that attached mid-batch has no other way to
    /// read the ones behind the front: the stream is a mirror with no backlog,
    /// and the request beside the answer's oneshot is what is left. A view
    /// draws the front.
    ///
    /// A draft leads because it is parked in its own registry rather than in
    /// the session's pending set, so a seat with no domain can still hold one.
    pub fn pending_asks(&self, slot: &SessionSlot) -> Vec<crate::protocol::PendingAsk> {
        let mut asks = Vec::new();
        {
            let parked = self.slack_drafts.lock();
            asks.extend(parked.values().filter(|(owner, _, _)| owner == slot).map(
                |(_, draft, _)| crate::protocol::PendingAsk::SlackDraft(Box::new(draft.clone())),
            ));
        }
        {
            let parked = self.browser_handoffs.lock();
            asks.extend(parked.values().filter(|(owner, _, _)| owner == slot).map(
                |(_, handoff, _)| {
                    crate::protocol::PendingAsk::BrowserHandOff(Box::new(handoff.clone()))
                },
            ));
        }
        if let Some(domain) = self.domain_session_for(slot) {
            let guard = domain.lock();
            asks.extend(
                guard
                    .pending_interactions
                    .values()
                    .map(crate::protocol::PendingInteractionSlot::ask),
            );
        }
        asks
    }

    /// What `entry`'s session is doing right now. The two liveness states
    /// that need no session of their own answer here; everything else is
    /// [`Self::session_activity`].
    ///
    /// Call this with no worker lock held - it reaches for
    /// `domain_handles` and then the `DomainSession`.
    pub fn worker_activity(
        &self,
        entry: &crate::mcp::workers::types::WorkerEntry,
    ) -> forge_primitives::SessionLifecycleState {
        use forge_primitives::{SessionLifecycleState as L, WorkerLiveness};

        // Neither has a connected session to interrogate.
        match entry.status {
            WorkerLiveness::Spawning => return L::Spawning,
            WorkerLiveness::Failed => return L::Failed,
            WorkerLiveness::Running => {}
        }
        self.session_activity(&entry.slot)
    }

    /// Why the last spawn or connection for `slot` failed, when it has not
    /// come up since. `None` for a slot that is running, was never tried,
    /// or recovered.
    pub fn spawn_failure(&self, slot: &SessionSlot) -> Option<String> {
        self.spawn_failures.lock().get(slot).cloned()
    }

    /// Record that `slot`'s spawn or connection failed, and why. Called
    /// before the failure releases the session, because the release is
    /// what takes the rest of the evidence with it.
    pub(crate) fn record_spawn_failure(&self, slot: &SessionSlot, message: &str) {
        self.spawn_failures.lock().insert(slot.clone(), message.to_owned());
    }

    /// Clear `slot`'s recorded failure: a session for it came up. Called
    /// from the `Connected` arm, which is the path production takes - the
    /// spawn entries hoist a domain straight into `domain_handles` rather
    /// than registering one, so clearing on registration would never run.
    pub(crate) fn clear_spawn_failure(&self, slot: &SessionSlot) {
        let _ = self.spawn_failures.lock().remove(slot);
    }

    /// What the session at `slot` is doing right now - the axis
    /// `WorkerLiveness` does not answer, since it stops moving once the
    /// worker connects. Reachable without a `WorkerEntry`, which a project
    /// lead has none of.
    ///
    /// A pending interaction outranks the turn it is blocking:
    /// `Attention` is the state a lead has to act on, and reporting the
    /// blocked worker as `Running` is what let the deadlock stay
    /// invisible.
    ///
    /// Call this with no worker lock held - it reaches for
    /// `domain_handles` and then the `DomainSession`.
    pub fn session_activity(&self, slot: &SessionSlot) -> forge_primitives::SessionLifecycleState {
        use forge_primitives::SessionLifecycleState as L;

        // Both of these are terminal for the session's own progress, so
        // they answer before anything about turns: a session waiting to
        // be let in, and one whose last attempt to start is still the
        // last thing that happened to it.
        if self.spawn_failure(slot).is_some() {
            return L::Failed;
        }
        let Some(domain) = self.domain_session_for(slot) else {
            return L::Sleeping;
        };
        if domain.lock().awaiting_login {
            return L::AuthRequired;
        }
        let blocked = {
            let guard = domain.lock();
            // A held interaction only matters while a turn can advance on
            // it, and this gate is what decides that for drafts too: one
            // parked on a slot with no turn (busytools/forge#672 - a slot
            // outliving its turn is incoherent state) reads `Idle` here
            // rather than as a person's to answer.
            if !guard.turn_in_flight() {
                return L::Idle;
            }
            // `RequiresAction` is the CLI naming its own block; a held slot
            // is forge naming it. Either way a human has to move first, and
            // calling that `Running` is what makes a blocked worker
            // invisible. This arm is reachable only because
            // `turn_in_flight()` counts `RequiresAction` as in-flight:
            // drop it from that OR and a `RequiresAction` session falls
            // through the gate above to `Idle` instead.
            matches!(
                guard.runtime_state,
                Some(forge_primitives::RuntimeSessionState::RequiresAction)
            ) || !guard.pending_interactions.is_empty()
        };
        // **A parked draft is a person's to answer too, and it lives
        // outside the set the arm above reads**: the draft registry is the
        // workspace's own (a seat with no domain can hold one, though the
        // gate above answers such a seat `Sleeping` before either arm
        // reaches here). Read after the guard comes off, un-nested, the way
        // `pending_asks` reads it - and without this a held draft leaves
        // the seat `Running`, which is what kept the client's
        // lifecycle-driven needs mark quiet while the dock sat unanswered
        // (#1758). **The browser hand-off is the same shape** - its own
        // registry, its own read - and the same silence without it (Ved,
        // 2026-10-07).
        if blocked || self.has_slack_draft(slot) || self.has_browser_hand_off(slot) {
            L::Attention
        } else {
            L::Running
        }
    }

    /// Whether `slot` is holding a parked Slack draft. The workspace's own
    /// registry rather than the session's pending set, which is what keeps a
    /// draft visible to `session_activity` when the set itself is empty.
    fn has_slack_draft(&self, slot: &SessionSlot) -> bool {
        self.slack_drafts.lock().values().any(|(owner, _, _)| owner == slot)
    }

    /// Whether `slot` is holding a parked browser hand-off, read exactly as
    /// the draft above: its own registry, so a held hand-off reaches
    /// `session_activity` and every needs mark the lifecycle drives.
    fn has_browser_hand_off(&self, slot: &SessionSlot) -> bool {
        self.browser_handoffs.lock().values().any(|(owner, _, _)| owner == slot)
    }

    /// `entry` projected to the wire shape with `activity` derived.
    /// This is the `agents__list` projection; `WorkerEntry::to_status`
    /// is the event-path one that leaves `activity` unset.
    pub fn worker_status_snapshot(
        &self,
        entry: &crate::mcp::workers::types::WorkerEntry,
    ) -> forge_primitives::WorkerStatus {
        forge_primitives::WorkerStatus {
            activity: Some(self.worker_activity(entry)),
            ..entry.to_status()
        }
    }

    /// The forge.toml project NAME that owns `cwd`, matched by the
    /// project whose expanded path is an ancestor-or-equal of `cwd`
    /// (component-aware, longest wins), so a worker in a
    /// `<project>/.claude/worktrees/<label>` worktree resolves to its
    /// parent project. `cwd` is `~`-expanded before the lexical match so
    /// a tilde form can't miss the already-expanded `ProjectView.path`.
    /// `None` when `cwd` is blank or under no configured project. The
    /// Inspector stamps this NAME onto a tab's UI bucket once (at
    /// Connect), then scopes SCHEDULES / GOTIFY by name rather than
    /// re-deriving the project every render tick from a stale cwd.
    pub fn project_name_for_path(&self, cwd: &str) -> Option<String> {
        if cwd.is_empty() {
            return None;
        }
        let cwd = crate::config::expand_home(cwd);
        self.list_projects()
            .into_iter()
            .filter(|view| cwd.starts_with(&view.path))
            .max_by_key(|view| view.path.as_os_str().len())
            .map(|view| view.name)
    }

    /// The `(org, project name)` a project key names, when forge.toml
    /// still declares it. The `sessions` table is keyed by the pair
    /// while every worker site holds the key, so a worker's row cannot be
    /// written or found without this translation.
    fn project_identity_for_key(&self, project_key: &ProjectKey) -> Option<(String, String)> {
        self.project_for_key(project_key).map(|project| (project.org, project.name))
    }

    /// Write `label`'s worker row: the id it runs under plus the
    /// re-spawn arguments its spawn stated. The row is the only thing a
    /// boot re-spawns from, so the charter, the kick, the interactive
    /// flag and the gitness its worktree derives from land here rather
    /// than staying in memory.
    ///
    /// `Err` when the store is closed, the project is no longer
    /// configured, or the write failed, so the caller can warn the lead
    /// that this worker will not survive a restart.
    pub(crate) fn record_worker_row(
        &self,
        project_key: &ProjectKey,
        label: &str,
        id: &str,
        charter: &str,
        kick: Option<&str>,
        resume_kick: Option<&str>,
        interactive: bool,
        is_git_repo: bool,
        mcp_families: Option<&[String]>,
    ) -> anyhow::Result<()> {
        let Some((org, project)) = self.project_identity_for_key(project_key) else {
            anyhow::bail!("no configured project for {}", project_key.as_str());
        };
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            anyhow::bail!("the session store is unavailable this session");
        };
        // A re-spawn passes no re-orient message of its own - it hands
        // the row's `resume_kick` to the child as the kick - so a `None`
        // here carries the stored value forward rather than erasing the
        // field its caller just read. Same for `kick`: only a first spawn
        // states one, and a resume that wrote its own live kick here would
        // replace the worker's original first turn with it.
        let existing = crate::store::sessions::get(db, &org, &project, label).ok().flatten();
        crate::store::sessions::put(
            db,
            &crate::store::sessions::SessionRecord {
                org,
                project,
                label: label.to_owned(),
                session_id: Some(id.to_owned()),
                charter: Some(charter.to_owned()),
                mcp_families: match mcp_families {
                    Some(names) if !names.is_empty() => Some(names.to_vec()),
                    // Empty resolves to every family, stored as absence;
                    // a resume states none and carries the stored value.
                    Some(_) => None,
                    None => existing.as_ref().and_then(|row| row.mcp_families.clone()),
                },
                kick: kick
                    .map(str::to_owned)
                    .or_else(|| existing.as_ref().and_then(|row| row.kick.clone())),
                resume_kick: resume_kick
                    .map(str::to_owned)
                    .or_else(|| existing.and_then(|row| row.resume_kick)),
                interactive: Some(interactive),
                is_git_repo: Some(is_git_repo),
            },
        )
    }

    /// The git flag the worker's row records, or `Ok(None)` when there is
    /// no row to read or its row predates the field. The spawn reads it in
    /// preference to probing the project path, so the cwd it builds is the
    /// one the launchpad already checked.
    ///
    /// A read failure is an `Err`, not an absent row: an absent row means
    /// the caller may probe, while an unreadable one means the answer is
    /// unknown and the caller should say so rather than quietly compose a
    /// different directory. Distinct from [`Self::stored_worker_row`],
    /// which reports both the same way because its callers act on the row
    /// itself rather than on a field of it.
    pub(crate) fn recorded_worker_is_git_repo(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) -> anyhow::Result<Option<bool>> {
        let Some((org, project)) = self.project_identity_for_key(project_key) else {
            return Ok(None);
        };
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            anyhow::bail!("the session store is unavailable this session");
        };
        Ok(crate::store::sessions::get(db, &org, &project, label)?.and_then(|row| row.is_git_repo))
    }

    /// The MCP-family selection the worker's row records, or `Ok(None)`
    /// when there is no row or its row names none (which resolves to
    /// every family). Same failure contract as
    /// [`Self::recorded_worker_is_git_repo`]: an unreadable row is an
    /// `Err`, not an absence.
    pub(crate) fn recorded_worker_mcp_families(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) -> anyhow::Result<Option<Vec<String>>> {
        let Some((org, project)) = self.project_identity_for_key(project_key) else {
            return Ok(None);
        };
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            anyhow::bail!("the session store is unavailable this session");
        };
        Ok(crate::store::sessions::get(db, &org, &project, label)?.and_then(|row| row.mcp_families))
    }

    /// Delete a worker's persisted row so it never re-spawns. The row is
    /// the only thing that brings one back, so a delete that cannot land
    /// is an error rather than a warn.
    ///
    /// `Ok` reports whether a row was there to remove. `Err` means the row
    /// could not be addressed or the delete failed - which is not the same
    /// answer as an absent row, so `spawn::handle_despawn_worker` reports
    /// the failure rather than NotFound when it gets one.
    pub(crate) fn delete_worker_row(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) -> anyhow::Result<bool> {
        // Every failure here is logged where it happens: the callers that
        // roll a spawn back have nothing to do with an `Err` and would
        // otherwise drop it silently.
        let Some((org, project)) = self.project_identity_for_key(project_key) else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                project = %project_key.as_str(),
                label = %label,
                "no configured project for this worker's key; its row is left where it is",
            );
            anyhow::bail!("no configured project for {}", project_key.as_str());
        };
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                event_name = "worker_row_delete_store_unavailable",
                project = %project_key.as_str(),
                label = %label,
                "the session store is unavailable; the worker's row is left where it is",
            );
            anyhow::bail!("the session store is unavailable this session");
        };
        match crate::store::sessions::delete(db, &org, &project, label) {
            Ok(existed) => Ok(existed),
            Err(error) => {
                tracing::error!(
                    target: "forge_workspace::workspace",
                    %error,
                    project = %project_key.as_str(),
                    label = %label,
                    "deleting a persisted worker failed; it may re-spawn on restart",
                );
                Err(error)
            }
        }
    }

    /// Every persisted worker row for `project_key`, the lead's own row
    /// excluded. Empty when the DB isn't open or the read fails. Backs
    /// the lead-reconnect re-spawn merge and the durable-identity read.
    pub(crate) fn worker_rows_for_project(
        &self,
        project_key: &ProjectKey,
    ) -> Vec<crate::store::sessions::SessionRecord> {
        let Some((org, project)) = self.project_identity_for_key(project_key) else {
            return Vec::new();
        };
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            return Vec::new();
        };
        crate::store::sessions::list_for_project(db, &org, &project)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    project = %project_key.as_str(),
                    "listing persisted workers failed",
                );
                Vec::new()
            })
            .into_iter()
            .filter(|row| row.label != forge_primitives::LEAD_LABEL)
            .collect()
    }

    /// Every persisted worker label the launchpad still offers, keyed by
    /// the project key it holds. A row the boot wave would not start is
    /// not among them.
    ///
    /// The launchpad's worker rows read this once per frame, which is
    /// what the shape is for: one read transaction answers every project
    /// row, over a projection that skips each row's charter. Unlike
    /// [`Self::list_live_workers`] it answers before the project has
    /// launched, which is when the launchpad renders.
    ///
    /// Empty on a read failure rather than surfacing it, deliberately
    /// unlike the sibling `stored_worker_row`: the caller is a render
    /// path, where a warn plus a bare row beats failing the frame.
    pub fn worker_labels_by_project(&self) -> HashMap<ProjectKey, Vec<String>> {
        let rows = {
            let guard = self.db.lock();
            let Some(db) = guard.as_ref() else {
                return HashMap::new();
            };
            crate::store::sessions::worker_row_index(db).unwrap_or_else(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "listing persisted worker labels failed",
                );
                Vec::new()
            })
        };
        let views = self.list_projects();
        let mut out: HashMap<ProjectKey, Vec<String>> = HashMap::new();
        for row in rows {
            if row.label == forge_primitives::LEAD_LABEL {
                continue;
            }
            // A row the wave would not start is not a worker the
            // launchpad can offer either - the same question, so the
            // pane and the wave answer it with one call.
            if let Some(view) = views.iter().find(|v| v.org == row.org && v.name == row.project)
                && crate::mcp::workers::types::worker_row_can_start(
                    &view.path,
                    &row.label,
                    row.is_git_repo,
                    row.session_id.is_some(),
                )
            {
                out.entry(view.key.clone()).or_default().push(row.label);
            }
        }
        out
    }

    /// The row `label` holds in `project_key`. Distinct from
    /// [`Self::worker_rows_for_project`], which swallows a read failure as
    /// empty: this surfaces the error (and treats a missing store as one)
    /// so the cron fire router can tell "conclusively absent" from "could
    /// not read" and leave the cron on a hiccup.
    pub(crate) fn stored_worker_row(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) -> anyhow::Result<Option<crate::store::sessions::SessionRecord>> {
        let Some((org, project)) = self.project_identity_for_key(project_key) else {
            anyhow::bail!("no configured project for {}", project_key.as_str());
        };
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            anyhow::bail!("the session store is unavailable this session");
        };
        crate::store::sessions::get(db, &org, &project, label)
    }

    /// Merge the supplied fields onto the worker row for `(project_key,
    /// label)`, leaving a `None` field at its stored value. Returns
    /// whether a row existed; this never creates one, because a row is
    /// what makes a worker re-spawn on the next lead connect. Read and
    /// write share one store lock so a concurrent despawn cannot land
    /// between them and resurrect the row.
    pub(crate) fn update_worker_row(
        &self,
        project_key: &ProjectKey,
        label: &str,
        charter: Option<String>,
        kick: Option<String>,
        resume_kick: Option<String>,
        mcp_families: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        let Some((org, project)) = self.project_identity_for_key(project_key) else {
            anyhow::bail!("no configured project for {}", project_key.as_str());
        };
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            anyhow::bail!("the session store is unavailable this session");
        };
        crate::store::sessions::update(
            db,
            &crate::store::sessions::SessionRecord {
                org,
                project,
                label: label.to_owned(),
                session_id: None,
                charter,
                kick,
                resume_kick,
                interactive: None,
                // `update` leaves an absent field alone; the gitness is
                // fixed at spawn and never re-decided here.
                is_git_repo: None,
                // Absent leaves the stored selection alone; `Some(empty)`
                // resets it to every family.
                mcp_families,
            },
        )
    }

    /// Scan the shared session-JSONL pool into a `UsageReport` for the
    /// `/usage` overlay. Query-style (a direct method, not a Command):
    /// reads the one real `projects` dir, refreshes the incremental
    /// per-file cache, and rolls the deduped summaries up into the four
    /// windows priced from the bundled table. Does blocking file IO, so
    /// callers run it off the UI thread.
    pub fn scan_usage(&self) -> forge_primitives::token_usage::UsageReport {
        use forge_agent::env::{timezone, token_usage};
        use time_tz::OffsetDateTimeExt;
        // Canonicalize so a symlinked projects tree is read once, not
        // once per alias.
        let projects_dir = forge_sdk::projects_dir_for(&self.config_dir);
        let projects_dir = std::fs::canonicalize(&projects_dir).unwrap_or(projects_dir);
        // Resolve the system timezone once so days bucket on the user's
        // wall clock, and derive "now" in the same zone for the windows.
        let tz = timezone::system_timezone();
        let summaries: Vec<_> = token_usage::usage_files(&projects_dir)
            .iter()
            .filter_map(|path| self.usage_summary_for(path, tz))
            .collect();
        let pricing = self.load_pricing();
        let now = time::OffsetDateTime::now_utc().to_timezone(tz);
        token_usage::roll_up(&summaries, &pricing, now)
    }

    /// Cached summary for `path` when its mtime and size still match,
    /// otherwise re-parse (bucketing by `tz`) and refresh the cache.
    /// `None` when the file vanished between listing and parsing.
    fn usage_summary_for(
        &self,
        path: &Path,
        tz: &time_tz::Tz,
    ) -> Option<forge_agent::env::token_usage::FileUsageSummary> {
        let key = path.to_string_lossy();
        let signature =
            std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        if let Some(mut cached) = self.load_usage_summary(&key)
            && signature.is_some_and(|(mtime, size)| cached.mtime == mtime && cached.size == size)
        {
            // An inactive session's mtime never changes, so a project
            // label guessed while its repo was absent would otherwise
            // outlive the checkout coming back.
            cached.refresh_unresolved_project(path);
            return Some(cached);
        }
        let parsed = forge_agent::env::token_usage::parse_file(path, tz)?;
        self.store_usage_summary(&key, &parsed);
        Some(parsed)
    }

    fn load_usage_summary(
        &self,
        path: &str,
    ) -> Option<forge_agent::env::token_usage::FileUsageSummary> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        crate::store::token_usage::load(db, path).unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                path = %path,
                "loading a usage summary failed",
            );
            None
        })
    }

    fn store_usage_summary(
        &self,
        path: &str,
        summary: &forge_agent::env::token_usage::FileUsageSummary,
    ) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::token_usage::store(db, path, summary)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                path = %path,
                "storing a usage summary failed",
            );
        }
    }

    /// Refresh the LiteLLM pricing cache when it is absent or older than
    /// a day, re-fetching immediately on a missed day. Fire-and-forget
    /// from the TUI: it fetches through the proxy-aware client and is a
    /// no-op on any network failure, so the last-good cache is kept.
    /// Returns whether a new price table was stored (so a caller can
    /// re-price without a redundant scan when nothing changed).
    /// The redb read and the ~1.6 MB write run on the blocking pool so
    /// the once-a-day fsync can't stall a UI frame.
    pub async fn refresh_pricing(self: &Arc<Self>) -> bool {
        let fresh = {
            let workspace = Arc::clone(self);
            tokio::task::spawn_blocking(move || workspace.pricing_is_fresh()).await.unwrap_or_else(
                |error| {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        %error,
                        "pricing freshness-check task failed; treating the cache as stale",
                    );
                    false
                },
            )
        };
        if fresh {
            return false;
        }
        let Some(json) = forge_agent::env::token_usage::pricing::fetch_litellm().await else {
            return false;
        };
        let workspace = Arc::clone(self);
        tokio::task::spawn_blocking(move || workspace.store_fresh_pricing(json))
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    "pricing store task failed; the cache was not updated",
                );
                false
            })
    }

    /// Store freshly-fetched pricing json unless it parses to an empty
    /// table - a garbage 200 must not wipe a good cache. Returns whether
    /// it stored.
    fn store_fresh_pricing(&self, json: String) -> bool {
        if forge_agent::env::token_usage::pricing::PricingTable::from_litellm_json(&json).is_empty()
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                "fetched pricing parsed to an empty table; keeping the existing cache",
            );
            return false;
        }
        self.store_pricing(&crate::store::pricing::CachedPricing {
            fetched_at: std::time::SystemTime::now(),
            json,
        });
        true
    }

    /// The cached pricing, or an empty table before the first fetch
    /// lands (the first `/usage` open renders tokens with a blank cost
    /// until then).
    fn load_pricing(&self) -> forge_agent::env::token_usage::pricing::PricingTable {
        use forge_agent::env::token_usage::pricing::PricingTable;
        let json = self.load_cached_pricing().map(|cached| cached.json);
        PricingTable::from_litellm_json(json.as_deref().unwrap_or("{}"))
    }

    /// Read the cached pricing snapshot, warning (not swallowing) on a
    /// redb or decode error so a corrupt cache is diagnosable.
    fn load_cached_pricing(&self) -> Option<crate::store::pricing::CachedPricing> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        crate::store::pricing::load(db).unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "loading the pricing cache failed",
            );
            None
        })
    }

    /// Whether the cached pricing is younger than the daily refresh
    /// window; a missing or older cache is stale and re-fetched.
    fn pricing_is_fresh(&self) -> bool {
        const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
        self.load_cached_pricing()
            .and_then(|cached| cached.fetched_at.elapsed().ok())
            .is_some_and(|age| age < REFRESH_INTERVAL)
    }

    fn store_pricing(&self, entry: &crate::store::pricing::CachedPricing) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::pricing::store(db, entry)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "storing the pricing cache failed",
            );
        }
    }

    /// The canonical model forge stamped for `key`'s session: the
    /// project's declared `model`. `None` when the session has no
    /// project registration or the project declares no model.
    pub(crate) fn canonical_model_for_session(&self, key: &SessionSlot) -> Option<String> {
        let project_name = self
            .pool
            .lock()
            .get(key)
            .and_then(|entry| entry.registration.as_ref())
            .map(|registration| registration.project.clone())?;
        self.config
            .projects
            .iter()
            .find(|project| project.name == project_name)
            .and_then(|project| project.model.clone())
    }

    /// The `/model` picker rows for a session: the declared models of
    /// the session's org's accounts, in pin order, deduped.
    pub(crate) fn declared_models_for_session(
        &self,
        key: &SessionSlot,
    ) -> Vec<forge_primitives::runtime::AvailableModel> {
        let org = self
            .pool
            .lock()
            .get(key)
            .and_then(|entry| entry.registration.as_ref())
            .map(|registration| registration.org.clone());
        let Some(org) = org else {
            return Vec::new();
        };
        // The org's account walk order, from any of its projects
        // (the pin is shared across the org).
        let mut order: Vec<String> = Vec::new();
        if let Some(project) = self.config.projects.iter().find(|p| p.org == org) {
            order.extend(project.accounts.iter().cloned());
            order.extend(project.fallback_accounts.iter().cloned());
        }
        let mut seen = std::collections::HashSet::new();
        let mut rows = Vec::new();
        for name in order {
            let Some(account) = self.config.accounts.iter().find(|a| a.display_name == name) else {
                continue;
            };
            for model in &account.models {
                if seen.insert(model.clone()) {
                    rows.push(forge_primitives::runtime::AvailableModel::new(
                        model.clone(),
                        model.clone(),
                    ));
                }
            }
        }
        rows
    }

    /// Install a redb store into a test workspace so the durable-vs-
    /// ephemeral persistence path is exercisable without `Workspace::new`.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn install_db_for_test(&self, db: crate::store::Db) {
        *self.db.lock() = Some(db);
    }

    /// Snapshot every live worker's session_key across every project.
    /// Used by the TUI's `live_worker_keys` to exclude worker buckets
    /// from project-row click routing without depending on
    /// `list_projects()` for enumeration.
    pub fn all_live_worker_session_keys(&self) -> Vec<SessionSlot> {
        self.live_workers
            .lock()
            .values()
            .flat_map(|entries| entries.iter().map(|e| e.slot.clone()))
            .collect()
    }

    /// Insert a worker entry into `live_workers[project_key]`.
    /// `remove_latest_worker` resolves the single live match. The
    /// one-live-worker-per-label invariant is enforced by
    /// [`Self::insert_live_worker_if_label_absent`]; this raw push is for
    /// callers (tests, re-tag) that already own that guarantee.
    pub fn insert_live_worker(
        &self,
        project_key: &ProjectKey,
        entry: crate::mcp::workers::types::WorkerEntry,
    ) {
        self.live_workers.lock().entry(project_key.clone()).or_default().push(entry);
    }

    /// Insert `entry` only if no live (non-`Failed`) worker already holds
    /// its label in `project_key`, and - when `cap` is `Some` - only if
    /// that project's live worker count is under it. Holds
    /// `live_workers.lock()` across the label-check, the cap-check AND
    /// the push, so two genuinely-concurrent SpawnWorker dispatches (a
    /// reconnect re-spawn racing a manual `agents__spawn`, say) can't
    /// both pass a check-then-insert window and fork two subprocesses
    /// onto one worktree or overshoot the cap. The label check precedes
    /// the cap check, so a duplicate-label spawn at the cap reports the
    /// collision. `cap: None` is the boot re-spawn exemption. Returns
    /// `Ok(())` on insert. This is the sole enforcement point for the
    /// at-most-one-live-worker-per-label invariant and the per-project
    /// worker cap.
    pub fn insert_live_worker_if_label_absent(
        &self,
        project_key: &ProjectKey,
        entry: crate::mcp::workers::types::WorkerEntry,
        cap: Option<usize>,
    ) -> Result<(), LiveWorkerRefusal> {
        // The cleanup marker is read while holding the live registry's lock,
        // in the same order `mark_despawn_cleanup_pending` writes it, so a
        // spawn cannot pass between "the label is free" and "the cleanup is
        // remembered". A despawn marks before it tears its entry down; from
        // then until the cleanup ends, this refuses.
        let mut workers = self.live_workers.lock();
        if self.despawn_cleanup_pending(project_key, &entry.label) {
            return Err(LiveWorkerRefusal::CleanupPending);
        }
        if let Some(existing) = workers.get(project_key).and_then(|entries| {
            crate::mcp::workers::types::live_worker_with_label(entries, &entry.label)
        }) {
            return Err(LiveWorkerRefusal::LabelLive(existing.slot.clone()));
        }
        if let Some(cap) = cap {
            let live = workers
                .get(project_key)
                .map_or(0, |entries| crate::mcp::workers::types::live_worker_count(entries));
            if live >= cap {
                return Err(LiveWorkerRefusal::AtCap { live, cap });
            }
        }
        workers.entry(project_key.clone()).or_default().push(entry);
        Ok(())
    }

    /// Mark `label`'s despawn cleanup as in flight in `project_key`, and
    /// hand back the guard that releases it.
    ///
    /// A despawn takes this BEFORE its teardown, under the live registry's
    /// lock: from here until the cleanup ends the label must not read as
    /// free to a spawn, because the worktree that cleanup will delete is
    /// still on disk. `Drop` does the releasing, so a cleanup that returns
    /// early or panics cannot leave the label unspawnable for the run.
    pub(crate) fn mark_despawn_cleanup_pending(
        self: &Arc<Self>,
        project_key: &ProjectKey,
        label: &str,
    ) -> DespawnCleanupPending {
        {
            let _live = self.live_workers.lock();
            self.despawn_cleanups.lock().insert((project_key.clone(), label.to_owned()));
        }
        DespawnCleanupPending {
            workspace: Arc::clone(self),
            project_key: project_key.clone(),
            label: label.to_owned(),
        }
    }

    /// Whether `label`'s despawn cleanup is still running in `project_key`.
    pub(crate) fn despawn_cleanup_pending(&self, project_key: &ProjectKey, label: &str) -> bool {
        self.despawn_cleanups.lock().contains(&(project_key.clone(), label.to_owned()))
    }

    /// The project's cap-relevant worker count: the same number
    /// `insert_live_worker_if_label_absent` enforces against, read for
    /// `WorkerFacade::capacity`.
    pub fn count_live_workers(&self, project_key: &ProjectKey) -> usize {
        self.live_workers
            .lock()
            .get(project_key)
            .map_or(0, |entries| crate::mcp::workers::types::live_worker_count(entries))
    }

    /// Remove the latest-spawned worker matching `label` from
    /// `live_workers[project_key]`. Returns the removed entry, or
    /// `None` when no match exists.
    pub fn remove_latest_worker(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) -> Option<crate::mcp::workers::types::WorkerEntry> {
        let mut map = self.live_workers.lock();
        let entries = map.get_mut(project_key)?;
        let last_match_idx = entries.iter().rposition(|e| e.label == label)?;
        Some(entries.remove(last_match_idx))
    }

    /// Remove the worker whose `session_key` exactly matches across
    /// any project's `live_workers`. Used by the async-spawn-failure
    /// path so concurrent same-label spawns don't accidentally
    /// roll back the wrong entry (`remove_latest_worker` would peek
    /// the wrong one when two workers share a label). Returns the
    /// matched `(project_key, entry)` pair, or `None` when no entry
    /// matches.
    pub fn remove_worker_by_session_key(
        &self,
        session_key: &SessionSlot,
    ) -> Option<(ProjectKey, crate::mcp::workers::types::WorkerEntry)> {
        let mut map = self.live_workers.lock();
        for (project_key, entries) in map.iter_mut() {
            if let Some(idx) = entries.iter().position(|e| e.slot == *session_key) {
                let entry = entries.remove(idx);
                return Some((project_key.clone(), entry));
            }
        }
        None
    }

    /// Drain every worker entry for `project_key` and return them in
    /// insertion order.
    pub fn drain_live_workers(
        &self,
        project_key: &ProjectKey,
    ) -> Vec<crate::mcp::workers::types::WorkerEntry> {
        self.live_workers.lock().remove(project_key).unwrap_or_default()
    }

    /// Locate `(project_key, label, is_git_repo_at_spawn, needs_tag)`
    /// for any worker matching `session_key` across every project's
    /// `live_workers`. Used by the Connected handler in
    /// `SessionTask::translate_event` to decide whether a just-
    /// connected session is a worker (and what tag to write).
    /// `is_git_repo_at_spawn` lets the tag-write path route to the
    /// worktree-derived JSONL via `worker_tag_dir` in
    /// `crate::mcp::workers::types`, and `needs_tag` says whether that
    /// write has ever landed - the rollback reads it to tell a worker
    /// that never established itself from one that is simply re-tagging.
    /// `None` when the session is a lead (or not a worker at all).
    pub fn worker_lookup_for_session(
        &self,
        session_key: &SessionSlot,
    ) -> Option<(ProjectKey, String, bool, bool)> {
        let workers = self.live_workers.lock();
        for (project_key, entries) in workers.iter() {
            if let Some(entry) = entries.iter().find(|e| e.slot == *session_key) {
                return Some((
                    project_key.clone(),
                    entry.label.clone(),
                    entry.is_git_repo_at_spawn,
                    entry.needs_tag,
                ));
            }
        }
        None
    }

    /// The working directory forge holds for `session_key`. Two
    /// sources, in order:
    /// 1. The sessions catalog (`session_cwd_for`). Leads only: the
    ///    boot scan hides worker-tagged sessions and the Connected
    ///    handler skips the catalog mirror for workers, so a worker
    ///    never has a row to read.
    /// 2. The worker registry (`live_workers` via
    ///    `worker_lookup_for_session`), composed against the project's
    ///    `forge.toml` path by `worker_tag_dir` - the worktree for a
    ///    git worker, the project root otherwise. This is the
    ///    authoritative source for every worker.
    ///
    /// `None` leaves the caller to decide what an unknown cwd means:
    /// `resume_cwd_for_slot` hands claude an empty cwd, while the
    /// review MCP reports `SessionCwdUnknown` to the caller.
    pub fn cwd_for_session(&self, session_key: &SessionSlot) -> Option<String> {
        if let Some(cwd) = self.session_cwd_for(session_key) {
            return Some(cwd);
        }
        let (project_key, label, is_git, _) = self.worker_lookup_for_session(session_key)?;
        let Some(root) = self.project_root_for_key(&project_key) else {
            // Unreachable while `forge.toml` and `live_workers` agree,
            // so treat a firing as drift rather than a normal miss.
            tracing::warn!(
                target: "forge_workspace::workspace",
                event_name = "session_cwd_registry_contradiction",
                slot = %session_key.display(),
                project_key = project_key.as_str(),
                worker_label = %label,
                "worker registry resolves this session but no loaded project matches its \
                 project_key, so no cwd can be composed",
            );
            return None;
        };
        Some(
            crate::mcp::workers::types::worker_tag_dir(&root, &label, is_git)
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Where a worker seat lists its own sessions from, or `None` for a
    /// lead.
    ///
    /// The directory is [`Self::cwd_for_session`]'s: a worker's worktree,
    /// composed from the live registry. The launching cwd is the wrong
    /// answer for a fresh git worker - it launches in the project root
    /// with `--worktree <label>`, so its transcripts land in the
    /// worktree's project dir and a listing read from the root finds none
    /// of them.
    fn worker_listing_for(&self, slot: &SessionSlot) -> Option<forge_agent::WorkerListing> {
        if slot.is_lead() {
            return None;
        }
        let dir = self.cwd_for_session(slot)?;
        Some(forge_agent::WorkerListing { label: slot.label().to_owned(), dir: dir.into() })
    }

    /// The cwd to pass `claude --resume` for the session at
    /// `session_key`: [`Self::cwd_for_session`], or an empty string
    /// when forge holds none (pass through and let the bridge surface
    /// ConnectionFailed - the session can't be resumed cleanly anyway).
    ///
    /// `claude --resume` does NOT receive a `--worktree` flag (see
    /// `SessionLaunchSettings::extra_args` in
    /// `forge-agent/src/client.rs` - lead/resume paths leave
    /// extra_args empty), so the subprocess cwd is the ONLY signal
    /// claude uses to derive the JSONL location. Handing a git
    /// worker just its project root makes claude look under the
    /// project's sanitised dir, miss the worker JSONL (which lives
    /// under the worktree's sanitised dir), and exit with "No
    /// conversation found with session ID:" (#245 Layer B).
    pub(crate) fn resume_cwd_for_slot(&self, session_key: &SessionSlot) -> String {
        self.cwd_for_session(session_key).unwrap_or_else(|| {
            tracing::warn!(
                target: "forge_workspace::workspace",
                slot = %session_key.display(),
                "resume_cwd_for_slot: no catalog cwd and no live worker entry; \
                 passing empty cwd to claude (resume will fail with ConnectionFailed)",
            );
            String::new()
        })
    }

    /// Look up a project root path by its `ProjectKey`. Searches
    /// the test overlay first (under `cfg(test)` / `feature =
    /// "testing"`), then `config.projects`. Returns `None` when no
    /// loaded project's path canonicalises to the given key.
    ///
    /// Used by [`Self::git_scan_cwd_for_session`] and
    /// [`Self::resume_cwd_for_slot`] to derive the project root
    /// from a worker's project_key without depending on the worker's
    /// `cwd_raw` value (which carries the project root for fresh
    /// spawns and the worktree path for resumed sessions - the two
    /// cases would otherwise need different composition logic).
    pub(crate) fn project_root_for_key(&self, target: &ProjectKey) -> Option<std::path::PathBuf> {
        self.project_for_key(target).map(|p| p.path)
    }

    /// The project whose path canonicalises to `target`. Consults the
    /// test overlay, like every other project lookup - a resolution
    /// that saw only `config.projects` would return `None` for a seeded
    /// project and make an absence assertion pass vacuously.
    /// The registry key for a project, resolved from its `forge.toml`
    /// name. `None` when the config no longer names it.
    pub(crate) fn project_key_for_name(&self, name: &str) -> Option<ProjectKey> {
        let project = self.find_project_view_by_name(name)?;
        Some(ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
            Some(&project.path.to_string_lossy()),
        )))
    }

    pub(crate) fn project_for_key(&self, target: &ProjectKey) -> Option<LoadedProject> {
        let derive_key = |project: &LoadedProject| {
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                &project.path.to_string_lossy(),
            )))
        };
        #[cfg(any(test, feature = "testing"))]
        {
            if let Some(project) =
                self.test_extra_projects.lock().iter().find(|p| &derive_key(p) == target).cloned()
            {
                return Some(project);
            }
        }
        // `None` when two projects share the key rather than the first
        // match: the key sanitises away punctuation and resolves
        // symlinks, so one repo declared under two org scopes collides.
        // Picking either would hand one project's env to the other's
        // sessions; no project env is the failure that cannot leak.
        let mut matches = self.config.projects.iter().filter(|p| &derive_key(p) == target);
        let first = matches.next()?;
        if matches.next().is_some() {
            tracing::warn!(
                target: "forge_workspace::workspace",
                event_name = "project_key_ambiguous",
                project_key = target.as_str(),
                "two projects resolve to this session-storage key, so neither project's \
                 settings are applied - give them distinct paths, or merge the entries if they \
                 are the same directory declared twice",
            );
            return None;
        }
        Some(first.clone())
    }

    /// Resolve the cwd a git-diff scan should run against for the
    /// session at `session_key`. Workers spawned in a git repo run
    /// inside claude's `--worktree <label>` fork at
    /// `<project_root>/.claude/worktrees/<label>/`, and the worker's
    /// `cwd_raw` carries different values depending on lifecycle:
    /// - fresh spawn -> `cwd_raw = <project_root>` (the value claude
    ///   sends in `AgentEvent::Connected.cwd` before it chdirs into
    ///   the worktree),
    /// - resumed session -> `cwd_raw = <project_root>/.claude/worktrees/<label>`
    ///   (claude chdirs before writing the first catalog row, so the
    ///   resume path reads the worktree path back as `cwd`).
    ///
    /// Anchor the composition on the worker's `project_key` (via
    /// [`Self::worker_lookup_for_session`] + the internal
    /// `project_root_for_key` lookup) rather than `cwd_raw` so both
    /// lifecycle states resolve to the same final path. For non-
    /// worker sessions (project leads), non-git workers, or projects
    /// whose root can't be resolved, returns `cwd_raw` unchanged.
    pub fn git_scan_cwd_for_session(
        &self,
        session_key: &SessionSlot,
        cwd_raw: &std::path::Path,
    ) -> std::path::PathBuf {
        let Some((project_key, label, is_git_repo_at_spawn, _)) =
            self.worker_lookup_for_session(session_key)
        else {
            // Trace-level so a real lookup-miss (race during
            // worker spawn, lead-session call) leaves a grep-able
            // trail without flooding normal logs. The lead-session
            // case is the common path and intentionally not
            // promoted higher.
            tracing::trace!(
                target: "forge_workspace::git_scan",
                slot = %session_key.display(),
                "no worker lookup; using cwd_raw unchanged"
            );
            return cwd_raw.to_path_buf();
        };
        if !is_git_repo_at_spawn {
            // Non-git workers don't fork into a worktree; they run
            // in the project root itself, so `cwd_raw` is already
            // the correct scan target.
            return cwd_raw.to_path_buf();
        }
        let Some(project_root) = self.project_root_for_key(&project_key) else {
            // Project lookup miss is structurally unusual (a worker
            // entry exists but its project_key doesn't resolve to a
            // loaded project) - log so a regression on the
            // forge.toml refresh path is visible, then fall back to
            // cwd_raw rather than synthesise a wrong path.
            tracing::warn!(
                target: "forge_workspace::git_scan",
                slot = %session_key.display(),
                project_key = project_key.as_str(),
                "worker entry present but project_root lookup missed; falling back to cwd_raw"
            );
            return cwd_raw.to_path_buf();
        };
        project_root.join(".claude/worktrees").join(&label)
    }

    /// Handle an async worker-spawn failure. Two paths, branching
    /// on the classifier outcome (#245 Layer C):
    ///
    /// - **Worktree-creation failure** (the worker never actually
    ///   started because git worktree setup failed): roll back the
    ///   `WorkerEntry` (the worker doesn't exist; the user-visible
    ///   signal is a typed notice routed to the lead's chat) and
    ///   emit `WorkerStatusChanged { Removed }`. Mirrors the sync
    ///   rollback in `handle_spawn_worker`.
    /// - **Any other failure** (resume against missing JSONL,
    ///   generic dispatch error, claude subprocess exit, etc.):
    ///   keep the `WorkerEntry` and transition it to
    ///   [`WorkerLiveness::Failed`](forge_primitives::WorkerLiveness::Failed)
    ///   with the first line of the message as the diagnostic. The Projects
    ///   pane renders the worker as a red `✕` with a DIM sub-row carrying the
    ///   diagnostic, so a stuck-Spawning-forever case becomes visible instead
    ///   of silently disappearing.
    ///
    /// The classifier uses `is_git_repo_at_spawn` + substring match
    /// against the bridge-wrapped message; see
    /// [`classify_worker_spawn_failure`] for the routing rules and
    /// the bridge-prefix contract.
    ///
    /// Returns `true` when the caller IS a worker (so it knows the
    /// failure was consumed here - the existing `ConnectionFailed`
    /// emission still fires for the TUI side). `false` for lead
    /// sessions or any other non-worker, in which case the caller's
    /// existing behaviour proceeds unchanged.
    ///
    /// [`classify_worker_spawn_failure`]: crate::mcp::workers::facade::classify_worker_spawn_failure
    pub(crate) fn handle_async_worker_spawn_failure(
        self: &Arc<Self>,
        session_key: &SessionSlot,
        message: &str,
        kind: forge_agent::client::SpawnFailureKind,
    ) -> bool {
        // Look up the worker entry WITHOUT removing it yet - the
        // worktree-failure path still removes (rollback semantics
        // for "worker never existed"); the general-failure path
        // transitions to Failed so the user sees what happened.
        //
        // A failed spawn reports the key it ran under, which is the one
        // its entry holds: every worker spawn, fresh or resumed, hands
        // both the same string.
        let Some((project_key, entry)) = ({
            let workers = self.live_workers.lock();
            workers.iter().find_map(|(project_key, entries)| {
                entries
                    .iter()
                    .find(|e| e.slot == *session_key)
                    .map(|entry| (project_key.clone(), entry.clone()))
            })
        }) else {
            return false;
        };
        // A payload parked for this worker's label has no session left to
        // drain it, so its bucket goes with the spawn.
        self.expire_parked_for_slot(
            &entry.slot,
            crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
        );
        // Classify against the entry's recorded is_git_repo_at_spawn
        // flag - same heuristic the sync agents__spawn path uses.
        let classified = crate::mcp::workers::facade::classify_worker_spawn_failure(
            message,
            entry.is_git_repo_at_spawn,
            kind,
        );
        if let crate::mcp::workers::facade::WorkerSpawnError::WorktreeCreationFailed { reason } =
            &classified
        {
            // Worktree-creation failure: roll back the worker entry
            // (the worker never existed; the user-visible signal is
            // the typed notice routed to the lead, not the worker row).
            self.remove_worker_by_session_key(&entry.slot);
            // Same release the tag-rollback arm runs: without it the
            // pool entry + command sender + domain handle + SessionTask
            // leak per failed fresh spawn, unbounded across retries.
            self.release_session(session_key);
            // A dynamic worker persisted its row on the optimistic spawn
            // reply, before this async failure. A worktree-creation
            // failure is a hard removal (the worker never started), so
            // delete the row too - otherwise it zombie-re-spawns every
            // restart despite a visibly-failed spawn. The
            // transition-to-Failed path below
            // deliberately keeps the row: a Failed-but-visible worker
            // wasn't despawned, so it should re-spawn to recover or
            // re-fail visibly.
            let _ = self.delete_worker_row(&project_key, &entry.label);
            // Notice goes to the lead session that spawned this
            // worker. Use the workspace's update channel + a fresh
            // Command::Prompt so the lead's claude subprocess sees
            // the envelope as a user turn and the TUI render path
            // picks up the bracketed prefix via forge_server::envelope::detect_inbound.
            let lead_slot = entry.spawned_by.clone();
            let pool_has_lead = self.pool.lock().contains_key(&lead_slot);
            if pool_has_lead {
                let wrapped = WrappedPrompt {
                    id: MessageId::mint(),
                    kind: WrappedKind::WorkerSpawnFailedNotice,
                    sender_name: entry.label.clone(),
                    sender_org: String::new(),
                    body: reason.clone(),
                };
                let uuid = forge_sdk::request_id::next_prompt_id();
                if let Err(err) = self.dispatch_workspace_prompt_under(
                    &lead_slot,
                    wrapped.to_prose(),
                    PromptSource::Peer,
                    uuid.clone(),
                ) {
                    tracing::warn!(
                        target: "forge_workspace::worker_async_failure",
                        project = %project_key.as_str(),
                        label = %entry.label,
                        error = ?err,
                        "WorkerSpawnFailedNotice dispatch to lead failed",
                    );
                }
                // And the visible echo, the way every other injected prompt
                // paints one: the CLI does not echo a prompt it was handed on
                // stdin, so a view that only drew the wire would show nothing
                // until the page reloaded.
                crate::spawn::push_peer_user_turn_into_chat(self, &lead_slot, &wrapped, &uuid);
            } else {
                tracing::warn!(
                    target: "forge_workspace::worker_async_failure",
                    project = %project_key.as_str(),
                    label = %entry.label,
                    slot = %lead_slot.display(),
                    "worker spawn failed but lead session is gone; dropping notice",
                );
            }
            // Emit WorkerStatusChanged::Removed - parity with the
            // sync rollback in handle_spawn_worker.
            let _ = self.update_tx.send(SessionUpdate::WorkerStatusChanged {
                project_key,
                action: crate::protocol::WorkerStatusAction::Removed,
                status: entry.to_status(),
                // Creating the worktree is what failed, and nothing
                // cleans up a partial one, so Absent is the only claim
                // that holds either way.
                worktree: crate::protocol::WorktreeDisposition::Absent,
            });
        } else {
            // Non-worktree failure (resume not found, generic
            // ConnectionFailed, etc.): transition to Failed via
            // Layer A's machinery. The worker entry stays visible
            // in `live_workers` + the Projects pane renders it as
            // `✕` with the captured message as the diagnostic
            // sub-row. Without this, the worker would vanish (as
            // it did pre-#245) and the user would be left wondering
            // why a team worker disappeared mid-flight.
            let diagnostic = message.lines().next().map(str::to_owned);
            transition_worker_to_failed(self, &project_key, &entry.slot, diagnostic);
        }
        true
    }

    /// Tag a worker session's JSONL with `forge:worker:<label>` and
    /// transition its `WorkerEntry` from `Spawning` to `Running`.
    /// Called from `SessionTask::translate_event` on the worker's first
    /// `Connected`.
    ///
    /// `cwd` is the worker's project path (the `directory` discriminator
    /// `tag_session` uses to find the right `.jsonl` when CONFIG_DIR
    /// hosts multiple project directories).
    ///
    /// The tag-write races against claude CLI's first JSONL write. For
    /// workers spawned with an `initial_prompt`, the file lands within
    /// ~100 ms-2 s of `Connected`, so a 30 x 100 ms retry loop reliably
    /// catches it. For idle-spawned workers (no prompt) claude doesn't
    /// create the JSONL at all until the first user turn arrives later,
    /// so the retry will exhaust with `Io(NotFound)`. That's NOT a
    /// rollback condition - we keep the worker live (it's fully
    /// functional from `live_workers`), transition to `Running`, mark
    /// the entry `needs_tag = true`, and emit `StatusChanged`. The
    /// opportunistic retry on first `DeliverWorkerPrompt`
    /// (see `spawn::handle_deliver_worker_prompt`) catches it once
    /// claude is processing the turn.
    ///
    /// Other errors (permission denied, disk full, invalid UUID) still
    /// indicate the worker can't be properly tracked on disk, so we
    /// roll back: remove from `live_workers`, release the session, emit
    /// a `Removed` status event.
    ///
    /// The tag-write runs in a detached tokio task to keep
    /// `translate_event` synchronous.
    ///
    /// Idempotent - calling twice for the same session_key is a no-op
    /// if the entry is already `Running`.
    pub(crate) fn apply_worker_tag_or_rollback(
        self: &Arc<Self>,
        session_key: &SessionSlot,
        session_id: &str,
        cwd: &str,
        minted_row: bool,
    ) {
        let Some((project_key, label, is_git_repo_at_spawn, needs_tag)) =
            self.worker_lookup_for_session(session_key)
        else {
            return;
        };
        // Resolve the config_dir for this session via the bridge so
        // the tag-write lands under the workspace's projects/ tree.
        let Some(config_dir) = self.config_dir_for(session_key) else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                slot = %session_key.display(),
                "apply_worker_tag: no agent registered; cannot resolve config_dir"
            );
            return;
        };
        self.apply_worker_tag_or_rollback_with_config_dir(
            session_key,
            session_id,
            &project_key,
            &label,
            cwd,
            is_git_repo_at_spawn,
            needs_tag,
            minted_row,
            &config_dir,
        );
    }

    /// Testable inner of [`Self::apply_worker_tag_or_rollback`] that
    /// takes the resolved `config_dir` directly. The production caller
    /// resolves via `config_dir_for`; unit tests pass a tempdir to
    /// exercise the retry / rollback / deferred branches without
    /// having to register a full `AgentHandle`.
    ///
    /// `cwd` is the project root from `forge.toml`; for git-repo
    /// workers (`is_git_repo_at_spawn = true`) the tag-write is
    /// routed to `<cwd>/.claude/worktrees/<label>` via
    /// [`crate::mcp::workers::types::worker_tag_dir`] so the lookup
    /// matches where claude's `--worktree <label>` actually wrote
    /// the JSONL.
    ///
    /// `needs_tag` and `minted_row` together decide whether a rollback
    /// takes the worker's durable row with it; see the arm below.
    pub(crate) fn apply_worker_tag_or_rollback_with_config_dir(
        self: &Arc<Self>,
        session_key: &SessionSlot,
        session_id: &str,
        project_key: &ProjectKey,
        label: &str,
        cwd: &str,
        is_git_repo_at_spawn: bool,
        needs_tag: bool,
        minted_row: bool,
        config_dir: &std::path::Path,
    ) {
        use tracing::Instrument;
        let workspace = Arc::clone(self);
        let project_key = project_key.clone();
        let session_key = session_key.clone();
        let session_id = session_id.to_owned();
        let label = label.to_owned();
        let effective_cwd = crate::mcp::workers::types::worker_tag_dir(
            std::path::Path::new(cwd),
            &label,
            is_git_repo_at_spawn,
        );
        let config_dir = config_dir.to_path_buf();
        let span = tracing::info_span!(
            "forge_workspace::worker_tag_write",
            slot = %session_key.display(),
            label = %label,
        );
        tokio::spawn(async move {
            let tag = forge_primitives::worker_tag(&label);
            let result = tag_session_with_retry(
                &config_dir,
                &session_id,
                &tag,
                &effective_cwd.to_string_lossy(),
                WORKER_TAG_RETRY_ATTEMPTS,
                WORKER_TAG_RETRY_DELAY,
            )
            .await;
            match result {
                Ok(()) => {
                    transition_worker_to_running(
                        &workspace,
                        &project_key,
                        &session_key,
                        TagWriteResult::Succeeded,
                    );
                }
                Err(forge_sdk::Error::Io(io_err))
                    if io_err.kind() == std::io::ErrorKind::NotFound =>
                {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        slot = %session_key.display(),
                        label = %label,
                        "tag_session_deferred: JSONL not yet on disk after retries; worker stays Running, tag will retry on first turn"
                    );
                    transition_worker_to_running(
                        &workspace,
                        &project_key,
                        &session_key,
                        TagWriteResult::DeferredNotFound,
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        slot = %session_key.display(),
                        label = %label,
                        error = ?err,
                        "tag_session failed for worker (non-NotFound); rolling back spawn"
                    );
                    let removed = workspace.remove_latest_worker(&project_key, &label);
                    if let Some(entry) = removed {
                        // The row goes with the worker only when all
                        // three hold: this connection is the first of a
                        // spawn that minted the row, and the worker never
                        // got as far as a tag.
                        //
                        // Every other shape of this arm - a resume, a
                        // boot re-spawn, a `/new` re-tag - runs over a
                        // row that pre-existed and holds the worker's
                        // charter, kick and the id being resumed, and the
                        // row is the worker rather than this spawn's
                        // leftover: deleting it loses a worker that only
                        // failed to write a JSONL tag. The `/new` case is
                        // why `minted_row` carries the first-connect half:
                        // a `/new` reuses this task and its domain, so the
                        // spawn's provenance is still stamped there, and
                        // only the Connected count tells the two apart.
                        //
                        // `needs_tag` is what separates those from the
                        // case this arm exists for. The spawn sets it and
                        // the first successful tag write clears it, so a
                        // row still carrying it belongs to a worker that
                        // never established itself on disk.
                        if minted_row && needs_tag {
                            let _ = workspace.delete_worker_row(&project_key, &label);
                        }
                        let worktree = crate::protocol::WorktreeDisposition::untouched(
                            entry.is_git_repo_at_spawn,
                        );
                        let _ = workspace.update_tx.send(SessionUpdate::WorkerStatusChanged {
                            project_key,
                            action: crate::protocol::WorkerStatusAction::Removed,
                            status: entry.to_status(),
                            worktree,
                        });
                    }
                    workspace.release_session(&session_key);
                }
            }
        }
        .instrument(span));
    }

    /// Opportunistically retry the JSONL tag-write for a worker whose
    /// `needs_tag` flag is set. Called by
    /// `spawn::handle_deliver_worker_prompt` when a `DeliverWorkerPrompt`
    /// arrives for a worker that was spawned idle (no `initial_prompt`)
    /// and therefore had no JSONL at `Connected`. By the time the first
    /// turn fires, claude is writing the JSONL, so the tag-write should
    /// succeed within a turn or two.
    ///
    /// On success: clear `needs_tag` and emit
    /// `WorkerStatusChanged { StatusChanged }`.
    ///
    /// On failure (any kind): log warn, leave `needs_tag = true` so the
    /// next turn retries again. Never rolls back - the worker stays
    /// functional regardless of the tag's on-disk state.
    pub(crate) fn retry_worker_tag_opportunistic(
        self: &Arc<Self>,
        project_key: &ProjectKey,
        session_key: &SessionSlot,
        session_id: &str,
        label: &str,
        cwd: &str,
        is_git_repo_at_spawn: bool,
    ) {
        let Some(config_dir) = self.config_dir_for(session_key) else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                slot = %session_key.display(),
                "retry_worker_tag: no agent registered; cannot resolve config_dir"
            );
            return;
        };
        self.retry_worker_tag_opportunistic_with_config_dir(
            project_key,
            session_key,
            session_id,
            label,
            cwd,
            is_git_repo_at_spawn,
            &config_dir,
        );
    }

    /// Testable inner of [`Self::retry_worker_tag_opportunistic`] that
    /// takes the resolved `config_dir` directly. Same shape as the
    /// `_with_config_dir` variant of `apply_worker_tag_or_rollback`,
    /// including the worktree-aware cwd routing for git-repo workers.
    pub(crate) fn retry_worker_tag_opportunistic_with_config_dir(
        self: &Arc<Self>,
        project_key: &ProjectKey,
        session_key: &SessionSlot,
        session_id: &str,
        label: &str,
        cwd: &str,
        is_git_repo_at_spawn: bool,
        config_dir: &std::path::Path,
    ) {
        use tracing::Instrument;
        let workspace = Arc::clone(self);
        let project_key = project_key.clone();
        let session_key = session_key.clone();
        let session_id = session_id.to_owned();
        let label = label.to_owned();
        let effective_cwd = crate::mcp::workers::types::worker_tag_dir(
            std::path::Path::new(cwd),
            &label,
            is_git_repo_at_spawn,
        );
        let config_dir = config_dir.to_path_buf();
        let span = tracing::info_span!(
            "forge_workspace::worker_tag_write",
            slot = %session_key.display(),
            label = %label,
        );
        tokio::spawn(async move {
            let tag = forge_primitives::worker_tag(&label);
            let result = tag_session_with_retry(
                &config_dir,
                &session_id,
                &tag,
                &effective_cwd.to_string_lossy(),
                WORKER_TAG_RETRY_ATTEMPTS,
                WORKER_TAG_RETRY_DELAY,
            )
            .await;
            match result {
                Ok(()) => {
                    let updated = {
                        let mut workers = workspace.live_workers.lock();
                        workers.get_mut(&project_key).and_then(|entries| {
                            entries.iter_mut().find(|e| e.slot == session_key).map(|entry| {
                                entry.needs_tag = false;
                                (entry.to_status(), entry.is_git_repo_at_spawn)
                            })
                        })
                    };
                    if let Some((status, is_git_repo_at_spawn)) = updated {
                        tracing::info!(
                            target: "forge_workspace::workspace",
                            slot = %session_key.display(),
                            label = %label,
                            "tag_session_retry: deferred tag-write succeeded on opportunistic retry"
                        );
                        let _ = workspace.update_tx.send(SessionUpdate::WorkerStatusChanged {
                            project_key,
                            action: crate::protocol::WorkerStatusAction::StatusChanged,
                            status,
                            worktree: crate::protocol::WorktreeDisposition::untouched(
                                is_git_repo_at_spawn,
                            ),
                        });
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        slot = %session_key.display(),
                        label = %label,
                        error = ?err,
                        "tag_session_retry: opportunistic retry failed; will try again on next turn"
                    );
                }
            }
        }
        .instrument(span));
    }

    // ---- Refresh helpers (workspace → agent) ----
    //
    // These five methods are query-style: TUI says "re-emit state X
    // for `key`" and the payload returns via `SessionUpdate`. They
    // bypass the [`Command`] envelope because they don't carry
    // mutation state, and they need the workspace-side
    // `Arc<AgentHandle>` lookup rather than per-session task routing.

    /// Request a fresh status snapshot for `key`. The bridge replies
    /// asynchronously via [`SessionUpdate::StatusSnapshot`].
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::UnknownSession`] when no agent is
    /// registered for `key` (e.g., the session was just closed) or
    /// when the bridge hasn't stamped a `session_id` yet.
    pub fn refresh_status_snapshot(&self, key: &SessionSlot) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.get_status_snapshot(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Request a fresh OAuth credentials snapshot for `key`. The
    /// bridge replies via [`SessionUpdate::OauthCredentialsSnapshot`].
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn refresh_oauth_credentials_snapshot(
        &self,
        key: &SessionSlot,
    ) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle
            .get_oauth_credentials_snapshot(sid)
            .map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Request a fresh context-usage snapshot for `key`. The bridge
    /// replies via [`SessionUpdate::ContextUsageSnapshot`].
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn refresh_context_usage(&self, key: &SessionSlot) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.get_context_usage(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Reload session plugins for `key`. The bridge replies via
    /// [`SessionUpdate::RuntimeReloadCompleted`] / `RuntimeReloadFailed`.
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn reload_plugins(&self, key: &SessionSlot) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.reload_plugins(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Request a fresh MCP server snapshot for `key`. The bridge
    /// replies via [`SessionUpdate::McpSnapshot`].
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn refresh_mcp_snapshot(&self, key: &SessionSlot) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.get_mcp_snapshot(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Record the OS walk of `key`'s process tree for views to read.
    ///
    /// The walk itself belongs to whoever owns the tick that drives it,
    /// because a walk is a `sysinfo` refresh that costs tens of
    /// milliseconds and no read should pay for one. The answer belongs
    /// here, so a second view reads the tree rather than walking the OS
    /// again. `None` clears it: the walk described a subprocess tree
    /// that is gone.
    ///
    /// No-op for a slot with no session: minting a domain for whoever
    /// asked would leave the workspace routing to a session nobody runs.
    pub fn store_process_snapshot(
        &self,
        key: &SessionSlot,
        snapshot: Option<forge_agent::env::processes::ProcessSnapshot>,
    ) {
        if let Some(domain) = self.domain_session_for(key) {
            domain.lock().process_snapshot = snapshot;
        }
    }

    /// The last OS walk of `key`'s process tree, or `None` when nothing
    /// has walked it or the tree it described is gone.
    pub fn process_snapshot(
        &self,
        key: &SessionSlot,
    ) -> Option<forge_agent::env::processes::ProcessSnapshot> {
        self.domain_session_for(key)?.lock().process_snapshot.clone()
    }

    // ---- Direct-accessor facades (workspace owns the bridge call) ----

    /// Resolve the auto-memory path the bridge would consult for
    /// `cwd`, scoped to `key`'s configured account. Returns `None`
    /// when no agent is registered for `key`.
    pub fn project_memory_path(&self, key: &SessionSlot, cwd: &std::path::Path) -> Option<PathBuf> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.project_memory_path(cwd))
    }

    /// Snapshot the bridge's settings documents for `key` at `cwd`.
    /// Returns `None` when no agent is registered for `key`.
    pub fn settings_documents(
        &self,
        key: &SessionSlot,
        cwd: Option<&std::path::Path>,
    ) -> Option<forge_agent::userdata::settings::SettingsDocuments> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.settings_documents(cwd))
    }

    /// The CLI's per-user preferences document, from `$HOME/.claude.json`.
    ///
    /// Read on each call rather than held, because the user can change a
    /// preference while forge runs.
    pub fn user_preferences(&self) -> Option<serde_json::Value> {
        #[cfg(any(test, feature = "testing"))]
        {
            if let Some(seeded) = self.test_user_preferences.lock().clone() {
                return Some(seeded);
            }
        }
        forge_agent::userdata::settings::user_preferences()
    }

    /// Resolve the agent's configured config_dir for `key`. Returns
    /// `None` when no agent is registered for `key`.
    pub fn config_dir_for(&self, key: &SessionSlot) -> Option<PathBuf> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.config_dir())
    }

    /// OS PID of the `claude` subprocess bound to `key`. Returns
    /// `None` when the session has no live client (pre-spawn /
    /// post-disconnect). The PID is stable
    /// for the lifetime of the subprocess, so consumers (e.g. the
    /// Inspector pane's PROCESSES OS walk) can cache snapshots
    /// keyed off this value.
    pub fn claude_pid(&self, key: &SessionSlot) -> Option<u32> {
        #[cfg(any(test, feature = "testing"))]
        {
            if let Some(seeded) = self.test_claude_pid.lock().get(key) {
                return Some(*seeded);
            }
        }
        self.agent_handle_for(key).and_then(|handle| handle.claude_pid())
    }

    /// Borrow the [`Arc<AgentHandle>`] registered against `key`.
    /// Workspace-internal helper - surfaces a sometimes-`None` to keep
    /// the early-init / disconnected branches explicit.
    fn agent_handle_for(&self, key: &SessionSlot) -> Option<Arc<AgentHandle>> {
        let pool = self.pool.lock();
        if let Some(pooled) = pool.get(key) {
            return Some(Arc::clone(&pooled.handle));
        }
        drop(pool);
        // Fall back to `domain_handles[key].conn` for pre-Connect /
        // testing-stub callers that never went through the pool path.
        let domain = self.domain_handles.lock().get(key).cloned()?;
        domain.lock().conn.clone()
    }

    /// Resolve `(handle, session_id_string)` for `key`. Both must be
    /// available; missing either surfaces as `UnknownSession` for
    /// uniform error handling.
    fn handle_and_session_id(
        &self,
        key: &SessionSlot,
    ) -> Result<(Arc<AgentHandle>, String), DispatchError> {
        let handle =
            self.agent_handle_for(key).ok_or_else(|| DispatchError::UnknownSession(key.clone()))?;
        let sid = self
            .domain_handles
            .lock()
            .get(key)
            .and_then(|d| d.lock().session_id.as_ref().map(std::string::ToString::to_string))
            .ok_or_else(|| DispatchError::UnknownSession(key.clone()))?;
        Ok((handle, sid))
    }
}

/// The repair line the 60 s poller logs under an auth-classified
/// failure, keyed on how the account authenticates. Credentials are
/// boot-frozen, so both classes point at the flat-key edit on the
/// account block, never a re-authentication of the shared config dir.
///
/// The base-url test must stay first: a global `[env]` setup token
/// reaches base-url accounts too, and the re-mint advice is for a
/// credential that account never reads.
fn auth_repair_hint(provider: forge_primitives::account::Provider) -> &'static str {
    if provider.uses_base_url() {
        "usage_poll fetch failed with auth error; fix the account's token and base_url keys and restart forge"
    } else {
        "usage_poll fetch failed with auth error; mint the setup token on the account block (claude setup-token) and restart forge"
    }
}

/// The [`crate::views::AccountBudget`] shape for an account, resolved
/// through its provider's forge-gateway backend. The stale-cache
/// refusal and its warn live on the backend's `budget`.
fn account_budget(
    account: &str,
    provider: forge_primitives::account::Provider,
    snapshot: Option<&forge_primitives::usage::UsageSnapshot>,
) -> crate::views::AccountBudget {
    let Some(backend) = forge_gateway::backend(provider) else {
        debug_assert!(false, "no backend registered for {provider:?}");
        return crate::views::AccountBudget::Unknown { spend_billed: false };
    };
    backend.budget(account, snapshot)
}

/// Map a failed probe to the renderer-facing
/// [`forge_gateway::UsageFetchStatus`] bucket. Separates HTTP 429 (the
/// common multi-instance throttle case) from the auth-related
/// failures (`Expired` / `NoCredentials` / `Unauthorized`) and
/// transport failures (`Network`), so the TUI's bottom-panel hint
/// can tell the user something specific rather than a generic
/// "fetch error". `Unmappable` never reaches the classifiers - both
/// callers handle a 200 that maps to nothing before classifying.
pub(crate) fn classify_oauth_usage_error(
    err: &forge_gateway::ProbeError,
) -> forge_gateway::UsageFetchStatus {
    use forge_gateway::UsageFetchStatus;
    use forge_primitives::usage::oauth::OauthUsageError;
    match err {
        forge_gateway::ProbeError::NoCredentials => UsageFetchStatus::Expired,
        forge_gateway::ProbeError::Unmappable(_) => UsageFetchStatus::Other,
        forge_gateway::ProbeError::Fetch(err) => match err {
            OauthUsageError::RateLimited { .. } | OauthUsageError::HttpStatus(429, _) => {
                UsageFetchStatus::RateLimited
            }
            OauthUsageError::Unauthorized(_) => UsageFetchStatus::Unauthorized,
            OauthUsageError::NoCredentials | OauthUsageError::Expired => UsageFetchStatus::Expired,
            OauthUsageError::Network(_) => UsageFetchStatus::NetworkFailed,
            OauthUsageError::UaProbe(_)
            | OauthUsageError::HttpStatus(_, _)
            // No probe converts a scope refusal - the token arm calls
            // /v1/messages, which has no scope refusal - and a 403
            // there is not an auth failure either.
            | OauthUsageError::ScopeInsufficient
            | OauthUsageError::Decode(_) => UsageFetchStatus::Other,
        },
    }
}

impl Workspace {
    /// Register a fresh `DomainSession` for `key` under this workspace.
    /// `handle` is `None` for pre-spawn / pre-Connect domains (filled
    /// in later when the spawn handler runs); `Some` for test fixtures
    /// that wire a stub handle up front.
    ///
    /// `DomainSession` carries workspace-internal routing metadata
    /// (`conn` / `session_id` / `pending_interactions`). TUI's per-
    /// session operational state lives on `UiSession`; the workspace
    /// does not read or write those fields.
    ///
    /// Returns the inserted `Arc<Mutex<DomainSession>>` so callers
    /// can seed fields on the same handle they just registered.
    pub fn register_domain_session(
        &self,
        key: SessionSlot,
        handle: Option<Arc<forge_agent::AgentHandle>>,
    ) -> Arc<Mutex<DomainSession>> {
        let domain = Arc::new(Mutex::new(DomainSession::new(key.clone(), handle)));
        let mut handles = self.domain_handles.lock();
        if handles.contains_key(&key) {
            // Overwriting silently drops the previous DomainSession
            // along with any pending_interactions oneshots - pending
            // permission/question round-trips would then deny with
            // "response channel closed" instead of completing. Log
            // loudly so a future programming error doesn't manifest
            // as a stale-deny that's hard to trace.
            tracing::error!(
                target: "forge_workspace::workspace",
                slot = %key.display(),
                "register_domain_session overwriting existing entry - pending interactions lost"
            );
        }
        handles.insert(key, Arc::clone(&domain));
        domain
    }

    /// Tell a sender its message never landed.
    ///
    /// The one delivery-ack path, and it is NOT outstanding state coming
    /// back: nothing is tracked between the send and this notice. A message
    /// parked for a sleeping project is dropped when the spawn it waited on
    /// fails, and the parked entry itself carries the sender, so the sender
    /// is told rather than left believing the words arrived.
    pub(crate) fn notice_undelivered_message(
        self: &Arc<Self>,
        sender: &SessionSlot,
        target: &SessionSlot,
        reason: crate::mcp::peers::types::PeerFailureReason,
    ) {
        // Body carries the human-readable failure reason - the sender's
        // chat block surfaces it underneath the bracket header.
        let body = match reason {
            crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed => {
                "target session connection lost".to_owned()
            }
        };
        let notice = WrappedPrompt {
            id: MessageId::mint(),
            kind: WrappedKind::DeliveryFailureNotice,
            sender_name: crate::mcp::peers::types::seat_name(target),
            sender_org: target.org().to_owned(),
            body,
        };
        // The CLI never echoes stdin-injected prompts back, so paint the
        // visible notice block ourselves before the LLM-side dispatch.
        let uuid = forge_sdk::request_id::next_prompt_id();
        crate::spawn::push_peer_user_turn_into_chat(self, sender, &notice, &uuid);
        if let Err(err) = self.dispatch_workspace_prompt_under(
            sender,
            notice.to_prose(),
            PromptSource::Peer,
            uuid,
        ) {
            tracing::warn!(
                target: "forge_workspace::workspace",
                slot = %sender.display(),
                error = ?err,
                "notice_undelivered_message: notice dispatch failed (sender closed?)"
            );
        }
    }
}

/// Discriminator for how a successful `apply_worker_tag_or_rollback`
/// arm decided to transition a worker to `Running`. The `Succeeded`
/// arm clears `needs_tag`; the `DeferredNotFound` arm keeps it set
/// so the opportunistic retry on first `DeliverWorkerPrompt` knows
/// to try again.
#[derive(Clone, Copy)]
enum TagWriteResult {
    /// Tag row was appended to the JSONL on disk.
    Succeeded,
    /// JSONL never appeared during the retry window. Worker stays
    /// live with `needs_tag = true` for opportunistic retry later.
    DeferredNotFound,
}

/// Shared transition: flip a worker's status to `Running`, update
/// `needs_tag` according to the tag-write outcome, and emit
/// `WorkerStatusChanged { StatusChanged }`. Idempotent.
fn transition_worker_to_running(
    workspace: &Arc<Workspace>,
    project_key: &ProjectKey,
    session_key: &SessionSlot,
    result: TagWriteResult,
) {
    let updated = {
        let mut workers = workspace.live_workers.lock();
        workers.get_mut(project_key).and_then(|entries| {
            entries.iter_mut().find(|e| e.slot == *session_key).map(|entry| {
                entry.status = forge_primitives::WorkerLiveness::Running;
                entry.needs_tag = matches!(result, TagWriteResult::DeferredNotFound);
                // Clear any stale diagnostic from a prior Failed
                // transition - the worker is alive again.
                entry.diagnostic = None;
                (entry.to_status(), entry.is_git_repo_at_spawn)
            })
        })
    };
    if let Some((status, is_git_repo_at_spawn)) = updated {
        let _ = workspace.update_tx.send(SessionUpdate::WorkerStatusChanged {
            project_key: project_key.clone(),
            action: crate::protocol::WorkerStatusAction::StatusChanged,
            status,
            worktree: crate::protocol::WorktreeDisposition::untouched(is_git_repo_at_spawn),
        });
    }
}

/// Shared transition: flip a worker's status to `Failed` with a
/// human-readable `diagnostic`, and emit `WorkerStatusChanged
/// { StatusChanged }`. Called by the `Connected`-never-arrived
/// paths (subprocess exit, ConnectionFailed event, resume rejected
/// by claude when the fall-through-to-fresh path declines to
/// retry).
///
/// Idempotent: when the entry is already `Failed` with an identical
/// diagnostic, this is a no-op (no mutation, no event emission). A
/// fresh diagnostic for an already-Failed entry DOES re-emit so the
/// UI picks up the new reason text.
///
/// Also clears `needs_tag` because a Failed worker won't be reached
/// by the opportunistic tag-retry path - leaving the flag set keeps
/// stale state on the entry the next time the worker resumes and
/// transitions back to Running.
///
/// The diagnostic should be the first line of claude's stderr (when
/// available) or the error variant name. Keep it short - the
/// Projects pane renders it as a one-row sub-line below the worker
/// label, truncated to the row's available width.
pub(crate) fn transition_worker_to_failed(
    workspace: &Arc<Workspace>,
    project_key: &ProjectKey,
    session_key: &SessionSlot,
    diagnostic: Option<String>,
) {
    let updated = {
        let mut workers = workspace.live_workers.lock();
        workers.get_mut(project_key).and_then(|entries| {
            entries.iter_mut().find(|e| e.slot == *session_key).and_then(|entry| {
                // Idempotency: same status + same diagnostic -> no-op.
                if entry.status == forge_primitives::WorkerLiveness::Failed
                    && entry.diagnostic == diagnostic
                {
                    return None;
                }
                entry.status = forge_primitives::WorkerLiveness::Failed;
                entry.diagnostic = diagnostic;
                // Clear needs_tag - a Failed worker won't reach the
                // opportunistic tag-retry path, and leaving the flag
                // set would keep stale state if the entry later
                // transitions back to Running on a successful resume.
                entry.needs_tag = false;
                Some((entry.to_status(), entry.is_git_repo_at_spawn))
            })
        })
    };
    if let Some((status, is_git_repo_at_spawn)) = updated {
        tracing::warn!(
            target: "forge_workspace::workspace",
            event_name = "worker_failed",
            project = %project_key.as_str(),
            slot = %session_key.display(),
            diagnostic = ?status.diagnostic,
            "worker session transitioned to Failed; row will render with diagnostic sub-line",
        );
        let _ = workspace.update_tx.send(SessionUpdate::WorkerStatusChanged {
            project_key: project_key.clone(),
            action: crate::protocol::WorkerStatusAction::StatusChanged,
            status,
            worktree: crate::protocol::WorktreeDisposition::untouched(is_git_repo_at_spawn),
        });
    }
}

/// Wrap `mutations::tag_session` with a retry loop scoped to the JSONL-
/// not-yet-on-disk race after `Connected`. claude CLI creates
/// `<session_id>.jsonl` lazily; until that lands, `find_session_file`
/// returns `None` and `tag_session` surfaces `Io(NotFound)`. We retry
/// only on that variant - any other error (permission denied, disk
/// full, invalid UUID, encode error) propagates immediately. The async
/// `tokio::time::sleep` is mandatory: this runs in a tokio task spawned
/// from `apply_worker_tag_or_rollback` (or `retry_worker_tag_opportunistic`),
/// never on a blocking thread.
async fn tag_session_with_retry(
    config_dir: &std::path::Path,
    session_id: &str,
    tag: &str,
    directory: &str,
    max_attempts: u32,
    delay: Duration,
) -> Result<(), forge_sdk::Error> {
    let mut last_err: Option<forge_sdk::Error> = None;
    for attempt in 0..max_attempts {
        match forge_agent::userdata::catalog::mutations::tag_session(
            config_dir,
            session_id,
            Some(tag),
            Some(directory),
        ) {
            Ok(()) => return Ok(()),
            Err(forge_sdk::Error::Io(io_err)) if io_err.kind() == std::io::ErrorKind::NotFound => {
                last_err = Some(forge_sdk::Error::Io(io_err));
                tracing::trace!(
                    target: "forge_workspace::workspace",
                    session_id = %session_id,
                    attempt = attempt + 1,
                    max_attempts,
                    "tag_session: JSONL not yet on disk, retrying"
                );
                tokio::time::sleep(delay).await;
            }
            Err(other) => return Err(other),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        forge_sdk::Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found after {max_attempts} attempts"),
        ))
    }))
}

/// Env for a spawn: the account env with `project`'s declared keys
/// merged over it. The spawn resolves the project first and refuses a
/// target that maps to none, so there is always one to merge.
fn session_env_for(
    project: &LoadedProject,
    account_env: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    // Logged even when the project declares nothing, so a spawn
    // carrying the wrong project's env is visible.
    tracing::info!(
        target: "forge_workspace::workspace",
        event_name = "session_env_project_applied",
        project = %project.name,
        keys = %crate::config::applied_env_keys(project),
        "`keys` lists what the project env contributed, empty when it declares none",
    );
    crate::config::session_env(project, account_env)
}

/// Stamp the project's `permission_mode` into the launch settings'
/// `permissions.defaultMode`. A spawn that resolved to no project
/// stamps nothing, so the launcher's session default applies.
fn apply_project_permission_mode(
    project: Option<&crate::config::LoadedProject>,
    settings: &mut SessionLaunchSettings,
) {
    if let Some(mode) = project.map(|project| project.permission_mode) {
        spawn::stamp_permission_mode(settings, mode);
    }
}

/// Stamp the project's canonical `model` into the launch settings, which
/// is how the session's model reaches the CLI. A project that declares
/// no model stamps nothing, so the caller's pin (forge defaults it to
/// the literal "opus") stands.
fn apply_project_model(
    project: Option<&crate::config::LoadedProject>,
    settings: &mut SessionLaunchSettings,
) {
    if let Some(model) = project.and_then(|project| project.model.as_deref()) {
        settings.settings = Some(pin_model_into_settings(settings.settings.take(), model));
    }
}

/// `settings` with `model` pinned to `model`, every other key kept.
fn pin_model_into_settings(settings: Option<serde_json::Value>, model: &str) -> serde_json::Value {
    let mut document = match settings {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    document.insert("model".to_owned(), serde_json::Value::String(model.to_owned()));
    serde_json::Value::Object(document)
}

/// Test helper: ensure `forge/` exists and return the production
/// `forge/forge.toml` path, so tests write where forge reads (not the
/// legacy top-level fallback).
#[cfg(test)]
fn forge_toml_path(config_dir: &std::path::Path) -> PathBuf {
    crate::config::ensure_forge_data_dir(config_dir).expect("forge/ dir").join("forge.toml")
}

/// Test helper: persist a worker row for `label` in `project_key` and
/// fail loudly when it did not land. `project_key` must resolve to a
/// configured project - the row is keyed by `(org, project name, label)`,
/// so a bare map key writes nothing and every assertion downstream would
/// then hold for the wrong reason.
#[cfg(test)]
fn seed_worker_row(ws: &Workspace, project_key: &ProjectKey, label: &str) {
    ws.seed_test_worker_row(project_key, label);
    assert!(
        ws.stored_worker_row(project_key, label).expect("read the seeded row").is_some(),
        "the worker row for {label} did not land; does {project_key:?} resolve to a project?",
    );
}

#[cfg(test)]
mod account_stamp_tests {
    use super::*;
    use crate::config::LoadedProject;

    fn project_with_mode(
        permission_mode: forge_primitives::permission::PermissionMode,
    ) -> LoadedProject {
        LoadedProject {
            name: "forge".to_owned(),
            path: PathBuf::from("/tmp/forge"),
            display_path: "~/Projects/forge".to_owned(),
            org: "Personal".to_owned(),
            accounts: vec!["Stargate".to_owned()],
            fallback_accounts: Vec::new(),
            auto_start: false,
            model: None,
            env: HashMap::new(),
            max_workers: None,
            issues: true,
            permission_mode,
        }
    }

    fn stamped_mode(settings: &SessionLaunchSettings) -> Option<String> {
        settings
            .settings
            .as_ref()
            .and_then(|s| s.get("permissions"))
            .and_then(|p| p.get("defaultMode"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    #[test]
    fn project_mode_stamps_fresh_spawn_settings_and_projectless_leaves_them() {
        let bypass =
            project_with_mode(forge_primitives::permission::PermissionMode::BypassPermissions);
        let mut stamped = SessionLaunchSettings::default();
        apply_project_permission_mode(Some(&bypass), &mut stamped);
        assert_eq!(
            stamped_mode(&stamped).as_deref(),
            Some("bypassPermissions"),
            "a project carrying permission_mode stamps the fresh spawn settings",
        );

        let mut untouched = SessionLaunchSettings::default();
        apply_project_permission_mode(None, &mut untouched);
        assert!(
            untouched.settings.is_none(),
            "a spawn that resolved to no project leaves fresh settings untouched"
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use forge_gateway::AccountStateMap;
    use std::fs;
    use tempfile::tempdir;

    /// Build a usage snapshot with 5h + shared-7d windows sharing one
    /// `resets_at`. Enough for the account snapshot tests.
    #[cfg(test)]
    fn account_usage_snapshot(
        five_hour: f64,
        seven_day: f64,
        resets_at: Option<std::time::SystemTime>,
    ) -> forge_primitives::usage::UsageSnapshot {
        use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
            five_hour: Some(UsageWindow {
                utilization: five_hour,
                resets_at,
                reset_description: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: seven_day,
                resets_at,
                reset_description: None,
            }),
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
            balance: None,
        }
    }

    /// A probe the test steers: a fixed snapshot, and a count of the calls
    /// it answered, so the scheduling around the probe is observable
    /// without a real `claude --version` or a network call.
    fn scripted_prober(
        calls: &Arc<std::sync::atomic::AtomicUsize>,
        snapshot: CliVersionInfo,
    ) -> CliVersionProber {
        let calls = Arc::clone(calls);
        Box::new(move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            Box::pin(std::future::ready(snapshot.clone()))
        })
    }

    /// The constructor's own launch is what starts the probe. Delete it and
    /// the core reads no version for the life of the process while every
    /// other test stays green, so nothing else notices that the feature's
    /// only production write is gone.
    #[tokio::test]
    async fn the_boot_construction_starts_the_probe_and_stores_what_it_reads() {
        let dir = make_workspace_dir();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probed = CliVersionInfo {
            installed: Some("2.1.156".to_owned()),
            latest: Some("2.1.201".to_owned()),
        };
        let workspace = Workspace::new_for_test_with_cli_version_prober(
            dir.path().to_owned(),
            scripted_prober(&calls, probed.clone()),
        )
        .expect("new");
        let mut woken = workspace.subscribe();

        let event = tokio::time::timeout(Duration::from_secs(5), woken.recv()).await;

        assert!(
            matches!(event, Ok(Some(SessionUpdate::CliVersionChanged))),
            "the constructor's launch probed and woke the views; without that launch the core reads no version for the life of the process",
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Acquire),
            1,
            "the boot launch probed exactly once",
        );
        assert_eq!(
            workspace.cli_version(),
            Some(probed),
            "and the store holds what the probe returned, not a snapshot that resolved nothing",
        );
    }

    /// A second start is a no-op. Two loops would each spawn
    /// `claude --version` and reach npm every five minutes, and each would
    /// write into the store the other one reads.
    #[tokio::test]
    async fn a_second_cli_version_probe_start_is_a_no_op() {
        let dir = make_workspace_dir();
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("new");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first = CliVersionInfo { installed: Some("2.1.156".to_owned()), latest: None };
        let second = CliVersionInfo { installed: Some("2.1.100".to_owned()), latest: None };

        workspace.start_cli_version_probe(scripted_prober(&calls, first.clone()));
        workspace.start_cli_version_probe(scripted_prober(&calls, second));
        let mut woken = workspace.subscribe();
        let event = tokio::time::timeout(Duration::from_secs(5), woken.recv()).await;
        assert!(
            matches!(event, Ok(Some(SessionUpdate::CliVersionChanged))),
            "the first start probed and woke the views",
        );

        // The second loop's first tick is immediate too, so an unguarded
        // start lands a second snapshot and a second event within
        // microseconds of the first.
        let again = tokio::time::timeout(Duration::from_millis(100), woken.recv()).await;
        assert!(
            again.is_err(),
            "the second start was a no-op; two loops would each spawn `claude --version` and reach npm",
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Acquire),
            1,
            "and only one probe call happened at all",
        );
        assert_eq!(
            workspace.cli_version(),
            Some(first),
            "the store holds the first probe's answer, which is what says the second never ran after it",
        );
    }

    /// A probe that came back empty on one side keeps the value an earlier
    /// probe resolved: a transient failure must not blank a version the
    /// views were already showing.
    #[test]
    fn an_empty_cli_version_probe_keeps_the_resolved_field() {
        let held = CliVersionInfo {
            installed: Some("2.1.156".to_owned()),
            latest: Some("2.1.201".to_owned()),
        };

        let merged = merge_cli_version(
            Some(&held),
            CliVersionInfo { installed: Some("2.1.156".to_owned()), latest: None },
        );
        assert_eq!(
            merged.latest.as_deref(),
            Some("2.1.201"),
            "a latest probe that failed must not wipe the latest an earlier probe found",
        );

        let merged = merge_cli_version(
            Some(&held),
            CliVersionInfo { installed: None, latest: Some("2.1.201".to_owned()) },
        );
        assert_eq!(
            merged.installed.as_deref(),
            Some("2.1.156"),
            "an installed probe that failed must not wipe the version already read",
        );
    }

    /// And a probe that did resolve a side replaces what was held, so the
    /// keep-the-old-value rule cannot pin the views to a stale answer.
    #[test]
    fn a_resolved_cli_version_probe_replaces_the_held_value() {
        let held = CliVersionInfo {
            installed: Some("2.1.156".to_owned()),
            latest: Some("2.1.201".to_owned()),
        };
        let next = CliVersionInfo {
            installed: Some("2.1.160".to_owned()),
            latest: Some("2.1.210".to_owned()),
        };

        assert_eq!(
            merge_cli_version(Some(&held), next.clone()),
            next,
            "a probe that resolved both sides replaces both, rather than pinning the held ones",
        );
    }

    /// Only a change wakes the views. The probe re-runs on a timer, so a
    /// re-probe landing the same snapshot must not wake them - both views
    /// would redraw for nothing every few minutes.
    #[test]
    fn only_a_changed_cli_version_wakes_the_views() {
        let store = Mutex::new(None);
        let update_tx = UpdateFanout::default();
        let mut woken = update_tx.subscribe(SubscriberRole::Answering);
        let first = CliVersionInfo { installed: Some("2.1.156".to_owned()), latest: None };

        store_cli_version(&store, &update_tx, first.clone());
        assert!(
            matches!(woken.try_recv(), Ok(SessionUpdate::CliVersionChanged)),
            "the first probe is a change from none",
        );

        store_cli_version(&store, &update_tx, first);
        assert!(woken.try_recv().is_err(), "an identical re-probe is not news");

        store_cli_version(
            &store,
            &update_tx,
            CliVersionInfo {
                installed: Some("2.1.156".to_owned()),
                latest: Some("2.1.201".to_owned()),
            },
        );
        assert!(
            matches!(woken.try_recv(), Ok(SessionUpdate::CliVersionChanged)),
            "a latest that appears where there was none is news",
        );
        assert_eq!(
            store.lock().as_ref().and_then(|held| held.latest.as_deref()),
            Some("2.1.201"),
            "and the store holds what the last merge resolved",
        );

        // A first probe that resolved nothing holds a snapshot and draws
        // what the views already had, so it is not news either.
        let unresolved = Mutex::new(None);
        store_cli_version(
            &unresolved,
            &update_tx,
            CliVersionInfo { installed: None, latest: None },
        );
        assert!(
            woken.try_recv().is_err(),
            "a first probe that resolved nothing draws what the views already had",
        );
        assert_eq!(
            unresolved
                .lock()
                .as_ref()
                .map(|held| (held.installed.as_deref(), held.latest.as_deref())),
            Some((None, None)),
            "the store holds what the probe read, and both sides are still empty",
        );
    }

    /// Every provider logs the repair line that can actually repair
    /// it. The base-url arm is load-bearing: a global `[env]` setup
    /// token reaches base-url accounts too, and a token-first order
    /// would send their 401 to the re-mint advice against a credential
    /// those providers never read.
    #[test]
    fn auth_repair_hint_keys_on_the_credential_shape() {
        use forge_primitives::account::Provider;

        for provider in [Provider::Codex, Provider::Openrouter, Provider::Zai] {
            assert_eq!(
                auth_repair_hint(provider),
                "usage_poll fetch failed with auth error; fix the account's token and \
                 base_url keys and restart forge",
                "{provider:?} is repaired by a flat-key edit",
            );
        }

        assert_eq!(
            auth_repair_hint(Provider::Anthropic),
            "usage_poll fetch failed with auth error; mint the setup token on the account \
             block (claude setup-token) and restart forge",
            "an anthropic account repairs through its setup token, token or not",
        );
    }

    /// `project_accounts_snapshot` returns one row per allow-list entry
    /// in order, each carrying its unusable reason, budget, fallback
    /// flag and boot loading state.
    #[test]
    fn project_accounts_snapshot_reports_allowlist_order_and_state() {
        let (ws, _rx) = Workspace::testing_stub();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        {
            let mut map = AccountStateMap::new(&[
                crate::config::LoadedAccount {
                    display_name: "A".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
                crate::config::LoadedAccount {
                    display_name: "B".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
            ]);
            // A: 5h saturated (100%, future reset) -> rate limited; 7d 63%.
            map.set_usage(
                &AccountKey("A".to_owned()),
                account_usage_snapshot(100.0, 63.0, Some(future)),
            );
            // B: usable (34% / 22%).
            map.set_usage(
                &AccountKey("B".to_owned()),
                account_usage_snapshot(34.0, 22.0, Some(future)),
            );
            ws.accounts.replace_state_for_test(map);
        }

        let rows = ws.project_accounts_snapshot(&["A".to_owned(), "B".to_owned()], &[]);

        assert_eq!(rows.len(), 2, "one row per allow-list entry");
        assert_eq!(rows[0].display_name, "A", "allow-list order preserved");
        assert_eq!(rows[1].display_name, "B");

        // A: saturated -> unusable as Saturated, carries a reset ETA.
        assert_eq!(
            rows[0].unusable,
            Some(forge_gateway::Unusable::Saturated),
            "A saturated on 5h -> Saturated, not a probe failure",
        );
        match rows[0].budget {
            crate::views::AccountBudget::Subscription {
                five_hour_util,
                seven_day_util,
                resets_at,
            } => {
                assert_eq!(five_hour_util, Some(100.0));
                assert_eq!(seven_day_util, Some(63.0));
                assert_eq!(resets_at, Some(future), "capped account shows when it unlocks");
            }
            ref other => panic!("a window-billed account renders as a subscription, got {other:?}"),
        }
        // B: under cap -> usable, no reset ETA.
        assert_eq!(rows[1].unusable, None, "B under cap on both windows");
        match rows[1].budget {
            crate::views::AccountBudget::Subscription { five_hour_util, resets_at, .. } => {
                assert_eq!(five_hour_util, Some(34.0));
                assert!(resets_at.is_none(), "usable account has no reset ETA");
            }
            ref other => panic!("a window-billed account renders as a subscription, got {other:?}"),
        }
    }

    /// An empty allow-list (project pins no accounts) falls back to
    /// every configured account in definition order; a `None`
    /// current-account marks no row.
    #[test]
    fn project_accounts_snapshot_empty_allowlist_falls_back_to_all_accounts() {
        let (ws, _rx) = Workspace::testing_stub();
        {
            let mut map = AccountStateMap::new(&[
                crate::config::LoadedAccount {
                    display_name: "One".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
                crate::config::LoadedAccount {
                    display_name: "Two".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
            ]);
            map.set_usage(&AccountKey("One".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            map.set_usage(&AccountKey("Two".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            ws.accounts.replace_state_for_test(map);
        }

        let rows = ws.project_accounts_snapshot(&[], &[]);
        let names: Vec<&str> = rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(names, vec!["One", "Two"], "empty pin lists all accounts in order");
        assert!(rows.iter().all(|r| r.unusable.is_none()), "both under cap -> usable");
    }

    /// Fallback rows are flagged against the org `fallback_accounts`
    /// list the caller passes in.
    #[test]
    fn project_accounts_snapshot_flags_fallback_rows() {
        let (ws, _rx) = Workspace::testing_stub();
        {
            let mut map = AccountStateMap::new(&[
                crate::config::LoadedAccount {
                    display_name: "A".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
                crate::config::LoadedAccount {
                    display_name: "B".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
            ]);
            map.set_usage(&AccountKey("A".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            map.set_usage(&AccountKey("B".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            ws.accounts.replace_state_for_test(map);
        }

        let rows = ws.project_accounts_snapshot(&["A".to_owned()], &["B".to_owned()]);

        assert_eq!(rows[0].display_name, "A", "regular rows lead");
        assert!(!rows[0].fallback, "a primary row is not flagged fallback");
        let fallbacks: Vec<&str> =
            rows.iter().filter(|r| r.fallback).map(|r| r.display_name.as_str()).collect();
        assert_eq!(fallbacks, vec!["B"], "the fallback row is flagged");
    }

    /// An account sitting in BOTH the allow-list and the fallback list
    /// renders once, flagged primary - the fallback list adds nothing
    /// for an account the pin already names.
    #[test]
    fn project_accounts_snapshot_lists_a_dual_listed_fallback_once_as_primary() {
        let (ws, _rx) = Workspace::testing_stub();
        {
            let mut map = AccountStateMap::new(&[
                crate::config::LoadedAccount {
                    display_name: "A".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
                crate::config::LoadedAccount {
                    display_name: "B".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
            ]);
            map.set_usage(&AccountKey("A".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            map.set_usage(&AccountKey("B".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            ws.accounts.replace_state_for_test(map);
        }

        let rows =
            ws.project_accounts_snapshot(&["A".to_owned(), "B".to_owned()], &["B".to_owned()]);

        assert_eq!(rows.len(), 2, "no duplicate row for the dual-listed account");
        assert_eq!(rows[1].display_name, "B");
        assert!(!rows[1].fallback, "a dual-listed account stays primary-flagged");
    }

    /// The read-only gateway view: every published org with its walk
    /// order and the live state of each account it names.
    #[test]
    fn gateway_view_reports_each_org_with_its_pins_and_account_state() {
        let (ws, _rx) = Workspace::testing_stub();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        {
            let mut map = AccountStateMap::new(&[
                crate::config::LoadedAccount {
                    display_name: "Ready".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
                crate::config::LoadedAccount {
                    display_name: "Capped".to_owned(),
                    provider: forge_primitives::account::Provider::Anthropic,
                    base_url: None,
                    models: vec!["claude-sonnet-5".to_owned()],
                    model_aliases: std::collections::HashMap::new(),
                    model_slugs: std::collections::HashMap::new(),
                    env: std::collections::HashMap::new(),
                },
            ]);
            map.set_usage(
                &AccountKey("Ready".to_owned()),
                account_usage_snapshot(34.0, 22.0, Some(future)),
            );
            map.set_usage(
                &AccountKey("Capped".to_owned()),
                account_usage_snapshot(100.0, 63.0, Some(future)),
            );
            ws.accounts.replace_state_for_test(map);
        }
        ws.gateway.set_org_pins([(
            "Default".to_owned(),
            forge_gateway::selection::OrgPin {
                accounts: vec!["Ready".to_owned()],
                fallback_accounts: vec!["Capped".to_owned()],
            },
        )]);

        let view = ws.gateway_view_snapshot();
        assert_eq!(view.len(), 1, "one block per published org");
        assert_eq!(view[0].org, "Default");
        assert_eq!(view[0].accounts, vec!["Ready".to_owned()], "the primary pin, in order");
        assert_eq!(view[0].fallback_accounts, vec!["Capped".to_owned()], "the fallback pin");
        assert_eq!(view[0].rows.len(), 2, "one row per account the org names");

        assert_eq!(view[0].rows[0].display_name, "Ready", "primaries read first");
        assert_eq!(view[0].rows[0].provider, forge_primitives::account::Provider::Anthropic);
        assert_eq!(view[0].rows[0].loading, forge_gateway::LoadingState::Ready);
        assert_eq!(view[0].rows[0].unusable, None, "a healthy account carries no reason tag");
        assert!(!view[0].rows[0].fallback);
        match view[0].rows[0].budget {
            crate::views::AccountBudget::Subscription {
                five_hour_util, seven_day_util, ..
            } => {
                assert_eq!(five_hour_util, Some(34.0), "the healthy account's 5h figure");
                assert_eq!(seven_day_util, Some(22.0), "and its 7d figure");
            }
            ref other => {
                panic!("a subscription account carries a subscription budget; got {other:?}")
            }
        }

        assert_eq!(view[0].rows[1].display_name, "Capped", "fallbacks read last");
        assert_eq!(
            view[0].rows[1].unusable,
            Some(forge_gateway::Unusable::Saturated),
            "a capped window is the why-not, not a probe failure",
        );
        assert!(view[0].rows[1].fallback, "the fallback list marks its rows");
    }

    /// The supersession guard that keeps a re-spawn intact: a stale
    /// predecessor task exiting must NOT wipe
    /// the successor's pool entry, command sender, or domain handle -
    /// all three are gated together on `Arc` identity. The current
    /// owner's own exit still releases all three.
    #[test]
    fn release_session_if_current_is_supersession_safe() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("sup-key");

        let (ha, _rxa) = Workspace::testing_stub_handle();
        let arc_a = Arc::new(ha);
        let (hb, _rxb) = Workspace::testing_stub_handle();
        let arc_b = Arc::new(hb);

        // The successor (account B) owns all three registrations.
        let (tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        ws.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::clone(&arc_b),
                account: AccountKey("B".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.command_senders.lock().insert(key.clone(), tx);
        ws.register_domain_session(key.clone(), Some(Arc::clone(&arc_b)));

        // The superseded predecessor (account A) exits and runs its
        // cleanup. Its handle no longer matches the pooled one, so the
        // guard no-ops across every map and B's live session survives.
        ws.release_session_if_current(&key, &arc_a);
        assert!(ws.pool.lock().contains_key(&key), "pool entry for B survives A's exit");
        assert!(ws.command_senders.lock().contains_key(&key), "command sender for B survives");
        assert!(ws.domain_handles.lock().contains_key(&key), "domain handle for B survives");

        // The current owner's own exit DOES release all three maps.
        ws.release_session_if_current(&key, &arc_b);
        assert!(!ws.pool.lock().contains_key(&key), "current owner removes the pool entry");
        assert!(!ws.command_senders.lock().contains_key(&key), "command sender removed");
        assert!(!ws.domain_handles.lock().contains_key(&key), "domain handle removed");
    }

    /// The relay is the workspace's and one per process, because two of them
    /// are two roles: the transport would register a host into one while
    /// every tool asked through the other, and every call would answer
    /// "no browser-capable client connected" with a client connected.
    #[test]
    fn the_browser_relay_is_one_and_starts_free() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());

        let first = ws.browser_relay();
        assert!(
            Arc::ptr_eq(&first, &ws.browser_relay()),
            "every caller reaches the same relay, or the role and the asks live in different ones",
        );
        let (to_host, _asks) = tokio::sync::mpsc::unbounded_channel();
        let (notices, _notice_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(first.register(1, to_host, notices), "no connection has taken the role at boot");
    }

    /// The board's edits reach the store through the command bus, and a
    /// refusal says so on the service line - a press that did nothing and
    /// a press the core refused must not read the same.
    #[tokio::test]
    async fn a_board_verdict_dispatches_and_a_refusal_says_so() {
        use forge_primitives::tasks::{By, Task, TaskId, TaskStatus};
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-board-dispatch");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let sample = |id: &str| Task {
            id: TaskId::from(id),
            project_name: "proj".to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status: TaskStatus::Pending,
            owner: None,
            // A child, so the approval completes it on the live board -
            // a root would close and archive, which is a different test.
            parent: Some(TaskId::from("epic")),
            waiting_on: None,
            estimate: None,
            rank: None,
            verify: Some(forge_primitives::tasks::Verify::User),
            links: Vec::new(),
            attempt: 0,
            archived_at: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        };
        ws.push_task(sample("t-1"));
        ws.update_task("proj", &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");

        ws.dispatch(Command::TaskVerdict {
            project: "proj".to_owned(),
            id: "t-1".to_owned(),
            approve: true,
            words: None,
        })
        .expect("app-level commands are accepted");
        let stored = ws.tasks_for_project("proj");
        assert_eq!(stored[0].status, TaskStatus::Completed, "the verdict landed");

        // The same verdict again names a row no longer waiting: the refusal
        // reaches the service line rather than only the log.
        ws.dispatch(Command::TaskVerdict {
            project: "proj".to_owned(),
            id: "t-1".to_owned(),
            approve: true,
            words: None,
        })
        .expect("accepted");
        let mut refused = false;
        while let Ok(update) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::ServiceStatus { message, .. } = update
                && message.contains("refused a verdict")
            {
                refused = true;
            }
        }
        assert!(refused, "a refused edit is said on the service line");
    }

    /// The rest of the board's edits say so too, the not-found case included:
    /// a rank or an assignment naming a row that has gone, and an assignment
    /// naming a project this forge does not carry, are presses whose silence
    /// would read as success.
    #[tokio::test]
    async fn a_board_edit_that_does_not_land_says_so() {
        use forge_primitives::tasks::TaskStatus;
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-board-misses");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        for (command, named) in [
            (
                Command::TaskRank {
                    project: "proj".to_owned(),
                    id: "gone".to_owned(),
                    to: crate::protocol::RankMove::Top,
                },
                "refused a re-order",
            ),
            (
                Command::TaskAssign {
                    project: "proj".to_owned(),
                    id: "gone".to_owned(),
                    owner: Some("lead".to_owned()),
                },
                "refused an assignment",
            ),
            (
                Command::TaskMove {
                    project: "proj".to_owned(),
                    id: "gone".to_owned(),
                    to: TaskStatus::InProgress,
                },
                "refused a move",
            ),
            (
                Command::TaskAssign {
                    project: "no-such-project".to_owned(),
                    id: "gone".to_owned(),
                    owner: None,
                },
                "refused an assignment",
            ),
        ] {
            ws.dispatch(command).expect("app-level commands are accepted");
            let mut said = false;
            while let Ok(update) = rx.try_recv() {
                if let crate::protocol::SessionUpdate::ServiceStatus { message, .. } = update
                    && message.contains(named)
                {
                    said = true;
                }
            }
            assert!(said, "{named} was not said on the service line");
        }
    }

    fn usage_workspace() -> (tempfile::TempDir, Arc<Workspace>) {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        (dir, ws)
    }

    #[test]
    fn scan_usage_rolls_up_lifetime_and_dedups() {
        let (dir, ws) = usage_workspace();
        let slug_dir = dir.path().join("projects").join("-slug");
        std::fs::create_dir_all(&slug_dir).expect("mkdir");
        let rec = |id: &str, model: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-07-08T09:30:34.184Z","message":{{"id":"{id}","model":"{model}","usage":{{"output_tokens":{out}}}}}}}"#
            )
        };
        // "a" appears twice (a resume re-log) and must count once; "b"
        // lands on a second model in the same project.
        std::fs::write(
            slug_dir.join("s.jsonl"),
            [rec("a", "m", 10), rec("a", "m", 10), rec("b", "n", 5)].join("\n"),
        )
        .expect("write");

        let report = ws.scan_usage();
        assert_eq!(report.lifetime.total.output, 15, "duplicate id counted once");
        let m = report.lifetime.by_model.iter().find(|r| r.label == "m").expect("m row");
        assert_eq!(m.output, 10, "the re-logged duplicate is not double-counted");
        assert_eq!(
            report.lifetime.by_model.iter().find(|r| r.label == "n").expect("n row").output,
            5,
        );
        assert_eq!(report.lifetime.by_project.len(), 1, "one project folds from one slug");
        assert_eq!(report.lifetime.by_project[0].output, 15);
    }

    #[test]
    fn scan_usage_reuses_cached_summary_for_unchanged_file() {
        let (dir, ws) = usage_workspace();
        let slug_dir = dir.path().join("projects").join("-slug");
        std::fs::create_dir_all(&slug_dir).expect("mkdir");
        std::fs::write(
            slug_dir.join("s.jsonl"),
            r#"{"type":"assistant","timestamp":"2026-07-08T00:00:00Z","message":{"id":"a","model":"m","usage":{"output_tokens":10}}}"#,
        )
        .expect("write");

        // First scan parses and caches the file.
        let _ = ws.scan_usage();

        // Poison the cache under the exact key scan_usage uses, keeping
        // the file's real mtime/size so the reuse condition holds. A
        // second scan returning the poison proves it did not re-parse.
        let canonical = std::fs::canonicalize(dir.path().join("projects")).expect("canon");
        let file = forge_agent::env::token_usage::usage_files(&canonical)
            .into_iter()
            .next()
            .expect("one file");
        let meta = std::fs::metadata(&file).expect("meta");
        let mut days = std::collections::BTreeMap::new();
        days.insert(
            "2026-07-08".to_owned(),
            forge_agent::env::token_usage::TokenCounts {
                output: 999,
                ..forge_agent::env::token_usage::TokenCounts::default()
            },
        );
        let mut by_model_day = std::collections::BTreeMap::new();
        by_model_day.insert("POISON".to_owned(), days);
        ws.store_usage_summary(
            &file.to_string_lossy(),
            &forge_agent::env::token_usage::FileUsageSummary {
                mtime: meta.modified().expect("mtime"),
                size: meta.len(),
                folded_project: "slug".to_owned(),
                project_resolved: true,
                by_model_day,
            },
        );

        let report = ws.scan_usage();
        assert!(
            report.lifetime.by_model.iter().any(|r| r.label == "POISON"),
            "an unchanged file reuses the cached summary instead of re-parsing",
        );
    }

    #[test]
    fn scan_usage_reparses_a_changed_file() {
        let (dir, ws) = usage_workspace();
        let slug_dir = dir.path().join("projects").join("-slug");
        std::fs::create_dir_all(&slug_dir).expect("mkdir");
        let path = slug_dir.join("s.jsonl");
        let rec = |id: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-07-08T09:30:34.184Z","message":{{"id":"{id}","model":"m","usage":{{"output_tokens":{out}}}}}}}"#
            )
        };
        std::fs::write(&path, rec("a", 10)).expect("write");
        assert_eq!(ws.scan_usage().lifetime.total.output, 10);

        // Appending a record grows the file, so the cached summary's size
        // no longer matches and the file must be re-parsed - otherwise
        // "usage never updates" until a restart.
        std::fs::write(&path, [rec("a", 10), rec("b", 5)].join("\n")).expect("rewrite");
        assert_eq!(
            ws.scan_usage().lifetime.total.output,
            15,
            "a changed file is re-parsed, not served stale from the cache",
        );
    }

    #[test]
    fn scan_usage_re_derives_only_an_unresolved_project_label() {
        let (dir, ws) = usage_workspace();
        let rec = r#"{"type":"assistant","timestamp":"2026-07-08T09:30:34.184Z","message":{"id":"a","model":"m","usage":{"output_tokens":10}}}"#;
        for slug in ["-guessed", "-settled"] {
            let slug_dir = dir.path().join("projects").join(slug);
            std::fs::create_dir_all(&slug_dir).expect("mkdir");
            std::fs::write(slug_dir.join("s.jsonl"), rec).expect("write");
        }
        let _ = ws.scan_usage();

        // Poison both cached labels, keeping each file's real mtime/size
        // so the reuse condition still holds. The unresolved row stands
        // for a label guessed while the repo was not checked out.
        let canonical = std::fs::canonicalize(dir.path().join("projects")).expect("canon");
        for file in forge_agent::env::token_usage::usage_files(&canonical) {
            let mut summary = ws
                .load_usage_summary(&file.to_string_lossy())
                .expect("the first scan cached this file");
            let guessed = file.to_string_lossy().contains("-guessed");
            summary.folded_project =
                if guessed { "GUESS-POISON" } else { "SETTLED-POISON" }.to_owned();
            summary.project_resolved = !guessed;
            ws.store_usage_summary(&file.to_string_lossy(), &summary);
        }

        let labels: Vec<String> =
            ws.scan_usage().lifetime.by_project.into_iter().map(|row| row.label).collect();
        assert!(
            labels.iter().any(|l| l == "guessed") && !labels.iter().any(|l| l == "GUESS-POISON"),
            "a guessed label is re-derived on cache reuse, so it heals: {labels:?}",
        );
        assert!(
            labels.iter().any(|l| l == "SETTLED-POISON"),
            "a settled label is trusted from cache, not re-derived: {labels:?}",
        );
    }

    #[test]
    fn pricing_is_fresh_only_within_the_daily_window() {
        let (_dir, ws) = usage_workspace();
        assert!(!ws.pricing_is_fresh(), "no cache is not fresh");
        ws.store_pricing(&crate::store::pricing::CachedPricing {
            fetched_at: std::time::SystemTime::now(),
            json: r#"{"m":{"input_cost_per_token":1,"output_cost_per_token":1}}"#.to_owned(),
        });
        assert!(ws.pricing_is_fresh(), "a just-now fetch is within the window");
        ws.store_pricing(&crate::store::pricing::CachedPricing {
            fetched_at: std::time::SystemTime::now()
                - std::time::Duration::from_secs(2 * 24 * 60 * 60),
            json: "{}".to_owned(),
        });
        assert!(!ws.pricing_is_fresh(), "a two-day-old fetch is stale and re-fetched");
    }

    // -- /model pricing cache ----------------------------------------

    #[test]
    fn store_fresh_pricing_keeps_a_good_cache_on_a_garbage_response() {
        let (_dir, ws) = usage_workspace();
        let good = r#"{"m":{"input_cost_per_token":0.001,"output_cost_per_token":0.002}}"#;
        assert!(ws.store_fresh_pricing(good.to_owned()), "a valid table stores");
        assert!(!ws.load_pricing().is_empty(), "the cache holds the priced model");
        // A garbage 200 parses empty and must NOT wipe the good cache.
        assert!(!ws.store_fresh_pricing("not json".to_owned()), "garbage is rejected");
        assert!(!ws.load_pricing().is_empty(), "the good cache survives the garbage response");
    }

    #[test]
    fn project_name_for_path_resolves_by_cwd_and_degrades_cleanly() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());

        let path = "/tmp/cron-project-path-proj";
        ws.seed_test_project("cronproj", path);

        // The escalation guard for the one-time Connected stamp: a clean
        // project-root cwd MUST resolve the name. If this ever returns
        // None the prefix logic itself is broken, not the input cwd.
        assert_eq!(
            ws.project_name_for_path(path).as_deref(),
            Some("cronproj"),
            "a clean project-root cwd resolves the project name",
        );
        assert_eq!(
            ws.project_name_for_path(&format!("{path}/.claude/worktrees/reviewer")).as_deref(),
            Some("cronproj"),
            "a worktree worker's cwd resolves to its parent project",
        );
        assert!(
            ws.project_name_for_path("/tmp/no-such-configured-project").is_none(),
            "a cwd mapping to no configured project resolves to no name",
        );
        assert!(ws.project_name_for_path("").is_none(), "a blank cwd resolves to no name");
    }

    /// Regression guard for symmetric matching: the project root exists
    /// on disk under a symlinked ancestor (`link` -> `real`, so the root
    /// canonicalizes to a different path) while the queried worktree
    /// subdir does NOT exist. A per-side canonicalize would resolve the
    /// root but leave the absent subdir lexical, so the two would stop
    /// sharing a prefix and the tab's SCHEDULES / GOTIFY would go blank.
    /// Lexical matching keeps both sides in the same form.
    #[test]
    #[cfg(unix)]
    fn project_name_for_path_resolves_absent_worktree_under_symlinked_root() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());

        std::fs::create_dir_all(dir.path().join("real").join("proj")).expect("create real root");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("real"), &link).expect("symlink");
        let root = link.join("proj");
        ws.seed_test_project("symproj", &root.to_string_lossy());

        let absent_worktree = root.join(".claude").join("worktrees").join("reviewer");
        assert_eq!(
            ws.project_name_for_path(&absent_worktree.to_string_lossy()).as_deref(),
            Some("symproj"),
            "an absent worktree subdir under a symlinked-ancestor root resolves to its parent",
        );
    }

    /// `SessionTarget::Default` is the alphabetically-first auto_start
    /// Buffer tracing output so the applied record can be read back.
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A stuck one-shot is re-evaluated on every 60s tick, so its warning
    /// has to be given once rather than once a minute for as long as the
    /// directory is missing. The repeat is still recorded, at debug, under
    /// the same `event_name`, so one filter still finds the whole story.
    ///
    /// The clearing is the half that matters. The state holds only the
    /// last pass's answer, so a cron that goes unwakeable AGAIN warns
    /// again; a set that never cleared would leave a durable cron that IS
    /// firing silent, which is the failure this family of changes exists
    /// to remove.
    ///
    /// Runs real fire passes under a log capture, which is why it sits
    /// beside `LogCapture` rather than with the other cron tests.
    #[test]
    fn a_stuck_cron_warns_once_then_warns_again_after_it_clears() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let (ws, _rx) = Workspace::testing_stub();
        let db_dir = tempdir().expect("db dir");
        ws.install_db_for_test(
            crate::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        let project_dir = tempdir().expect("project dir");
        ws.seed_test_project("proj", &project_dir.path().to_string_lossy());
        let key = ws.project_key_for_name("proj").expect("seeded project");
        // A git worker whose worktree is not there, so the wave would skip
        // it and no fire can land.
        ws.record_worker_row(&key, "steward", "steward-uuid", "c", None, None, false, true, None)
            .expect("seed the stranded row");

        // A whole minute, so the recurring's next `*/5` slot is at least a
        // minute later and the middle pass below cannot catch it.
        let t0 = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(600_000_000);
        for (id, kind) in
            [("once", CronKind::Once(t0)), ("again", CronKind::Recurring("*/5 * * * *".to_owned()))]
        {
            ws.push_cron(CronEntry {
                id: CronId::from(id),
                project_name: "proj".to_owned(),
                kind,
                prompt: "p".to_owned(),
                created_at: std::time::SystemTime::UNIX_EPOCH,
                description: None,
                last_fire: None,
                next_fire: t0,
                team_role: Some("steward".to_owned()),
            });
        }

        let log_of = |capture: &LogCapture| String::from_utf8_lossy(&capture.0.lock()).into_owned();
        let warns = |capture: &LogCapture| -> usize {
            log_of(capture)
                .lines()
                .filter(|line| line.contains("cron_owner_cannot_be_woken") && line.contains("WARN"))
                .count()
        };
        let pass = |capture: &LogCapture, now: std::time::SystemTime| {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(capture.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || ws.fire_due_crons(now));
        };

        let first = LogCapture::default();
        pass(&first, t0);
        assert_eq!(
            warns(&first),
            2,
            "both crons are newly unwakeable, so both warn: {}",
            log_of(&first)
        );

        // Half a minute on: the one-shot is still stuck and still due, and
        // the recurring has advanced past this pass entirely.
        let second = LogCapture::default();
        pass(&second, t0 + std::time::Duration::from_secs(30));
        assert_eq!(
            warns(&second),
            0,
            "a cron that was already unwakeable last pass does not warn again: {}",
            log_of(&second),
        );
        assert!(
            log_of(&second).contains("cron_owner_cannot_be_woken"),
            "and it is still recorded, at debug, under the same event_name: {}",
            log_of(&second),
        );

        // A day on, the recurring is due again with its owner still
        // stranded: this time that is new again.
        let third = LogCapture::default();
        pass(&third, t0 + std::time::Duration::from_secs(86_400));
        assert_eq!(
            warns(&third),
            1,
            "a cron unwakeable again after a pass that did not see it must warn again - a \
             set that never cleared would leave a firing cron silent: {}",
            log_of(&third),
        );
    }

    /// The applied record names the keys a project contributed and must
    /// never carry their values - it is always-on, so a widened field
    /// writes tokens to disk on every spawn. Asserted on a DIRECT call:
    /// the record is emitted in this crate before any subprocess, so
    /// this needs no binary and no wait.
    #[test]
    fn the_applied_record_logs_key_names_and_never_a_value() {
        const SENTINEL: &str = "value-must-never-be-logged";
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("solo");
        fs::create_dir_all(&root).expect("root");
        let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
        fs::write(
            forge_dir.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "solo"
path = "{root}"
env = {{ SOLO_TOKEN = "value-must-never-be-logged" }}
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
                root = root.display()
            ),
        )
        .expect("write forge.toml");
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        let project = config.projects[0].clone();

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt().with_writer(capture.clone()).finish();
        tracing::subscriber::with_default(subscriber, || {
            session_env_for(&project, &std::collections::HashMap::new());
        });
        let log = String::from_utf8_lossy(&capture.0.lock()).into_owned();

        assert!(log.contains("SOLO_TOKEN"), "the record names the key: {log}");
        assert!(!log.contains(SENTINEL), "and never its value: {log}");
    }

    /// Two projects at one path collide on the session-storage key, so
    /// neither can be told apart: an ambiguous target resolves to NO
    /// project rather than the first match's, which is what refuses the
    /// spawn rather than handing it one twin's env and the other's pin.
    /// Second assertion is the control: an unambiguous project still
    /// resolves, so the refusal above is about the twins and not about
    /// the lookup being broken for every target.
    #[test]
    fn an_ambiguous_storage_key_resolves_to_no_project() {
        let dir = tempdir().expect("tempdir");
        let shared = dir.path().join("shared");
        let solo = dir.path().join("solo");
        fs::create_dir_all(&shared).expect("shared");
        fs::create_dir_all(&solo).expect("solo");
        let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
        fs::write(
            forge_dir.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "twin-a"
path = "{shared}"
env = {{ TWIN_TOKEN = "twin-a-secret" }}
[[orgs.projects]]
name = "twin-b"
path = "{shared}"
[[orgs.projects]]
name = "solo"
path = "{solo}"
env = {{ SOLO_TOKEN = "solo-secret" }}

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
                shared = shared.display(),
                solo = solo.display()
            ),
        )
        .expect("write forge.toml");
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_owned(), config)
            .expect("the stub config's [[slack]] entries are well-formed");
        let key = |p: &std::path::Path| {
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                &p.to_string_lossy(),
            )))
        };

        let _ = key(&shared);
        let ambiguous = SessionTarget::FreshInProject {
            slot: SessionSlot::worker("TestOrg", "no-such-project", "minted-twin-id"),
        };
        assert!(
            ws.project_for_target(&ambiguous).is_none(),
            "a slot naming no declared project resolves to none, which refuses the spawn",
        );

        let solo_target = SessionTarget::Named("solo".to_owned());
        assert_eq!(
            ws.project_for_target(&solo_target).map(|project| project.name),
            Some("solo".to_owned()),
            "an unambiguous project still resolves with nothing on disk yet",
        );
    }

    /// The case where cwd has nothing to read: a resume of a session
    /// whose transcript is gone. The slot names the project, so the
    /// project resolves from `forge.toml` with an empty catalog - there
    /// is no transcript to carry it.
    #[test]
    fn a_resume_with_no_transcript_resolves_its_project() {
        let dir = tempdir().expect("tempdir");
        let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
        fs::write(
            forge_dir.join("forge.toml"),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "gone"
path = "/tmp/gone"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_owned(), config)
            .expect("the stub config's [[slack]] entries are well-formed");
        assert!(ws.catalog.lock().is_empty(), "no transcript was ever scanned");
        let key = SessionSlot::lead("Personal", "gone");

        assert_eq!(
            ws.project_for_target(&SessionTarget::Session(key)).map(|p| p.name),
            Some("gone".to_owned()),
            "the project resolves from the slot, with no transcript to read it from",
        );
    }

    /// An update merges only the supplied fields onto the stored row and
    /// survives the redb round-trip. It must never create a row: a row
    /// means the worker should be alive, so an absent one reports "not
    /// updated" rather than bringing a worker into existence at the next
    /// lead connect.
    #[test]
    fn update_worker_row_merges_only_supplied_fields() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/update-worker-row");
        let project = ws.project_key_for_name("forge").expect("seeded project");
        ws.record_worker_row(
            &project,
            "steward",
            "steward-test-id",
            "original charter",
            Some("original kick"),
            Some("original resume"),
            false,
            false,
            None,
        )
        .expect("seed the row this test then updates");
        let stored = |ws: &Arc<Workspace>| {
            ws.worker_rows_for_project(&project)
                .into_iter()
                .find(|w| w.label == "steward")
                .expect("row present")
        };

        assert!(
            ws.update_worker_row(
                &project,
                "steward",
                Some("new charter".to_owned()),
                None,
                None,
                None
            )
            .expect("update succeeds"),
            "an existing row reports updated",
        );
        let row = stored(&ws);
        assert_eq!(row.charter.as_deref(), Some("new charter"), "the supplied field changed");
        assert_eq!(row.kick.as_deref(), Some("original kick"), "an absent field is untouched");
        assert_eq!(
            row.resume_kick.as_deref(),
            Some("original resume"),
            "an absent field is untouched",
        );

        // The other two fields update independently, and the charter set
        // above persists across a second call.
        assert!(
            ws.update_worker_row(
                &project,
                "steward",
                None,
                Some("new kick".to_owned()),
                Some("new resume".to_owned()),
                None,
            )
            .expect("second update succeeds"),
        );
        let row = stored(&ws);
        assert_eq!(row.charter.as_deref(), Some("new charter"), "the earlier update survived");
        assert_eq!(row.kick.as_deref(), Some("new kick"));
        assert_eq!(row.resume_kick.as_deref(), Some("new resume"));

        // The family selection follows the same contract: supplied
        // replaces, absent keeps, empty resets to every family.
        assert!(
            ws.update_worker_row(
                &project,
                "steward",
                None,
                None,
                None,
                Some(vec!["cron".to_owned()])
            )
            .expect("families update succeeds"),
        );
        assert_eq!(stored(&ws).mcp_families, Some(vec!["cron".to_owned()]));
        assert!(
            ws.update_worker_row(&project, "steward", None, None, None, None)
                .expect("absent families update succeeds"),
        );
        assert_eq!(stored(&ws).mcp_families, Some(vec!["cron".to_owned()]), "absent keeps");
        assert!(
            ws.update_worker_row(&project, "steward", None, None, None, Some(Vec::new()))
                .expect("reset succeeds"),
        );
        assert_eq!(stored(&ws).mcp_families, None, "an empty list resets to every family");

        assert!(
            !ws.update_worker_row(&project, "ghost", Some("c".to_owned()), None, None, None)
                .expect("absent row is not an error"),
            "no row means not updated",
        );
        assert!(
            ws.worker_rows_for_project(&project).iter().all(|w| w.label != "ghost"),
            "a failed update must not create the row",
        );
    }

    fn live_worker_entry(label: &str, key: &str) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.to_owned(),
            charter: "c".to_owned(),
            slot: SessionSlot::from_str_for_test(key),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// The Projects-pane close (`handle_close_worker` -> `teardown_worker`)
    /// deletes the persisted dynamic-worker row so it never re-spawns,
    /// scoped to the closed label - siblings survive.
    #[tokio::test]
    async fn projects_pane_close_deletes_persisted_dynamic_worker_row() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/pane-close-forge");
        let project = ws.project_key_for_name("forge").expect("seeded project");

        seed_worker_row(&ws, &project, "reviewer");
        seed_worker_row(&ws, &project, "tester");
        ws.insert_live_worker(&project, live_worker_entry("reviewer", "worker-1"));
        ws.insert_live_worker(&project, live_worker_entry("tester", "worker-2"));

        crate::spawn::handle_close_worker(&ws, &project, "reviewer");

        let rows = {
            let guard = ws.db.lock();
            crate::store::sessions::list_for_project(
                guard.as_ref().expect("db installed"),
                "TestOrg",
                "forge",
            )
            .expect("list")
        };
        let labels: Vec<&str> = rows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["tester"], "close deletes only the closed worker's row");
    }

    /// The `agents__despawn` path (`handle_despawn_worker` ->
    /// `teardown_worker`) deletes the persisted dynamic-worker row too.
    #[tokio::test]
    async fn mcp_despawn_deletes_persisted_dynamic_worker_row() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/despawn-forge");
        let project = ws.project_key_for_name("forge").expect("seeded project");

        seed_worker_row(&ws, &project, "reviewer");
        ws.insert_live_worker(&project, live_worker_entry("reviewer", "worker-1"));

        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::spawn::handle_despawn_worker(&ws, &project, "reviewer", false, tx);
        assert!(matches!(rx.await, Ok(crate::protocol::DespawnResult::Despawned { .. })));

        let rows = {
            let guard = ws.db.lock();
            crate::store::sessions::list_for_project(
                guard.as_ref().expect("db installed"),
                "TestOrg",
                "forge",
            )
            .expect("list")
        };
        assert!(rows.is_empty(), "despawn deletes the persisted dynamic-worker row");
    }

    fn worker_cron(
        id: &str,
        project: &str,
        team_role: Option<&str>,
    ) -> forge_primitives::CronEntry {
        forge_primitives::CronEntry {
            id: forge_primitives::CronId::from(id),
            project_name: project.to_owned(),
            kind: forge_primitives::CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: team_role.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn teardown_worker_drops_its_crons_and_subs_keeps_others() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/cron-teardown-dyn");
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");
        ws.insert_live_worker(&view_key, live_worker_entry("scratch", "worker-1"));

        let scratch_cron = worker_cron("scratch-cron", "forge", Some("scratch"));
        let lead_cron = worker_cron("lead-cron", "forge", None);
        let sibling_cron = worker_cron("sibling-cron", "forge", Some("reviewer"));
        ws.push_cron(scratch_cron.clone());
        ws.push_cron(lead_cron.clone());
        ws.push_cron(sibling_cron.clone());
        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        let lead_sub = gotify_sub("forge", &[], None);
        ws.add_gotify_subscription(scratch_sub.clone(), true);
        ws.add_gotify_subscription(lead_sub.clone(), true);

        crate::spawn::handle_close_worker(&ws, &view_key, "scratch");

        let crons = ws.crons_for_project("forge");
        assert!(
            crons.iter().all(|c| c.id != scratch_cron.id),
            "the dynamic worker's cron is dropped"
        );
        assert!(crons.iter().any(|c| c.id == lead_cron.id), "the lead cron survives");
        assert!(crons.iter().any(|c| c.id == sibling_cron.id), "a sibling worker's cron survives");
        let persisted_crons = {
            let guard = ws.db.lock();
            crate::store::cron::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted_crons.iter().all(|c| c.id != scratch_cron.id),
            "the cron is dropped from the store too",
        );
        assert!(
            persisted_crons.iter().any(|c| c.id == lead_cron.id),
            "the lead cron is still stored"
        );

        let subs = ws.gotify_subscriptions_for_project("forge");
        assert!(subs.iter().all(|s| s.id != scratch_sub.id), "the dynamic worker's sub is dropped");
        assert!(subs.iter().any(|s| s.id == lead_sub.id), "the lead sub survives");
    }

    /// #3: persisting reports failure (rather than swallowing it) when
    /// the store is unavailable, so the MCP spawn path can warn the lead
    /// that the worker won't survive a restart.
    #[test]
    fn record_worker_row_errors_when_store_unavailable() {
        let (ws, _rx) = Workspace::testing_stub();
        // The project resolves, so the only thing left to fail on is the
        // store: without this seed an unresolvable key would error too and
        // the assertion would hold for the wrong reason.
        ws.seed_test_project("forge", "/tmp/row-no-store");
        let project = ws.project_key_for_name("forge").expect("seeded project");
        // No install_db_for_test: the store is closed for this session.
        let error = ws
            .record_worker_row(
                &project, "reviewer", "id", "charter", None, None, false, false, None,
            )
            .expect_err("a closed store must surface a durability failure, not a silent no-op");
        assert!(
            error.to_string().contains("store is unavailable"),
            "and it names the store as the reason, got: {error}",
        );
    }

    fn gotify_sub(
        project: &str,
        applications: &[&str],
        min_priority: Option<u8>,
    ) -> forge_primitives::GotifySubscription {
        forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: project.to_owned(),
            team_role: None,
            applications: applications.iter().map(|s| (*s).to_owned()).collect(),
            min_priority,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn gotify_notif(
        app: &str,
        title: &str,
        message: &str,
        priority: u8,
    ) -> crate::GotifyNotification {
        crate::GotifyNotification {
            app: app.to_owned(),
            title: title.to_owned(),
            message: message.to_owned(),
            priority,
        }
    }

    /// Drain every currently-queued `SessionUpdate` from the test rx.
    fn drain_updates(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::protocol::SessionUpdate>,
    ) -> Vec<crate::protocol::SessionUpdate> {
        let mut out = Vec::new();
        while let Ok(u) = rx.try_recv() {
            out.push(u);
        }
        out
    }

    /// Each caller of `subscribe()` gets a stream of its own, so a second
    /// view attaches beside the first instead of being refused the one
    /// receiver. Catches a revert to the single-take slot, where the
    /// second caller receives nothing.
    #[tokio::test]
    async fn subscribe_hands_every_caller_its_own_stream() {
        let dir = make_workspace_dir();
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("new");
        let mut tui = workspace.subscribe();
        let mut web = workspace.subscribe();

        assert!(
            workspace.update_tx().send(SessionUpdate::CatalogLoaded),
            "a subscribed workspace delivers",
        );

        assert!(
            matches!(tui.try_recv(), Ok(SessionUpdate::CatalogLoaded)),
            "the first subscriber receives the update",
        );
        assert!(
            matches!(web.try_recv(), Ok(SessionUpdate::CatalogLoaded)),
            "the second subscriber receives the same update",
        );
    }

    /// `dispatch_workspace_prompt` is the queue-signal discriminator:
    /// an idle session receives the plain prompt and no
    /// `PromptQueuedWhileBusy`; a session with a turn in flight gets
    /// the signal carrying its key ahead of the same dispatch. The
    /// intercept buffers before real routing stamps `turn_pending`,
    /// so the idle fire does not self-arm - the explicit stamp is the
    /// discriminator.
    #[test]
    fn dispatch_workspace_prompt_signals_only_when_turn_in_flight() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("qkey", "/tmp/q-dispatch");
        let cwd = project_expanded_path(&ws, "qkey");
        ws.record_connected_session(&cwd, "q-uuid", None);
        let key = SessionSlot::from_str_for_test("q-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&key, "q-uuid");
        ws.enable_test_dispatch_intercept();

        ws.dispatch_workspace_prompt(&key, "idle".to_owned()).expect("idle dispatch");
        assert!(
            !drain_updates(&mut rx)
                .iter()
                .any(|u| matches!(u, SessionUpdate::PromptQueuedWhileBusy { .. })),
            "an idle dispatch must not signal PromptQueuedWhileBusy",
        );

        ws.domain_session_for(&key).expect("domain").lock().turn_pending = true;
        ws.dispatch_workspace_prompt(&key, "queued".to_owned()).expect("busy dispatch");
        let signalled = drain_updates(&mut rx)
            .into_iter()
            .any(|u| matches!(u, SessionUpdate::PromptQueuedWhileBusy { key: k } if k == key));
        assert!(signalled, "a turn-in-flight dispatch signals PromptQueuedWhileBusy with the key");
    }

    /// A prompt forge wrote itself and no view drew - a worker kick, an
    /// auto-continue - draws as the words the model received: the CLI does
    /// not echo a prompt back, and nothing else carries them. The delivery
    /// path is the other half of the pair: a delivery's words are drawn by
    /// the envelope update its own caller emits, so a bare frame beside it
    /// would draw the same turn twice.
    #[test]
    fn a_forged_prompt_draws_its_words_where_a_delivery_prompt_forges_none() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("fkey", "/tmp/f-forged-prompt");
        let cwd = project_expanded_path(&ws, "fkey");
        ws.record_connected_session(&cwd, "f-uuid", None);
        let key = SessionSlot::from_str_for_test("f-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&key, "f-uuid");
        ws.enable_test_dispatch_intercept();

        ws.dispatch_forged_prompt(&key, "get on with it".to_owned()).expect("forged dispatch");
        let drawn = drain_updates(&mut rx);
        let frame = drawn.iter().find_map(|u| match u {
            SessionUpdate::ChatAppended { msg, origin, .. } => Some((msg, origin)),
            _ => None,
        });
        let Some((msg, origin)) = frame else {
            panic!("a forged prompt draws its words; got {drawn:?}")
        };
        assert!(origin.is_none(), "no view drew these words, so the frame claims no origin");
        let Message::User { message, .. } = msg else {
            panic!("a forged prompt draws as the user turn the model received, got {msg:?}")
        };
        assert!(
            matches!(
                message.content.first(),
                Some(forge_primitives::ContentBlock::Text { text, .. }) if text == "get on with it"
            ),
            "the frame carries the prose the model received: {message:?}",
        );

        ws.dispatch_workspace_prompt(&key, "[Cron]\n\nrun the summary".to_owned())
            .expect("delivery dispatch");
        assert!(
            !drain_updates(&mut rx).iter().any(|u| matches!(u, SessionUpdate::ChatAppended { .. })),
            "a delivery's words are drawn by its own envelope update, so this path forges no frame",
        );
    }

    /// A busy forged prompt signals the queue before it draws.
    ///
    /// The frame follows a dispatch that landed, and the signal comes off that
    /// same dispatch, so the order a view reads is "queued" and then the
    /// words - the reverse would draw a turn for a prompt still on its way.
    #[test]
    fn a_busy_forged_prompt_signals_before_it_draws() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("bkey", "/tmp/b-forged-prompt");
        let cwd = project_expanded_path(&ws, "bkey");
        ws.record_connected_session(&cwd, "b-uuid", None);
        let key = SessionSlot::from_str_for_test("b-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&key, "b-uuid");
        ws.enable_test_dispatch_intercept();
        ws.domain_session_for(&key).expect("domain").lock().turn_pending = true;

        ws.dispatch_forged_prompt(&key, "get on with it".to_owned()).expect("busy dispatch");

        let order: Vec<&str> = drain_updates(&mut rx)
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::PromptQueuedWhileBusy { .. } => Some("signal"),
                SessionUpdate::ChatAppended { .. } => Some("frame"),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            ["signal", "frame"],
            "the queue signal precedes the frame the words draw as",
        );
    }

    /// A forged prompt that never landed draws nothing: the words did not
    /// reach a model, so a frame for them would show a turn nothing answers.
    #[test]
    fn a_refused_forged_prompt_draws_nothing() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let gone = SessionSlot::from_str_for_test("no-such-seat");

        let refused = ws.dispatch_forged_prompt(&gone, "get on with it".to_owned());

        assert!(refused.is_err(), "a seat nothing holds refuses the dispatch");
        assert!(
            !drain_updates(&mut rx).iter().any(|u| matches!(u, SessionUpdate::ChatAppended { .. })),
            "a refused prompt never reached a model, so nothing draws its words",
        );
    }

    /// A seat with a live domain session whose dispatch is captured rather
    /// than routed, plus the workspace's update stream. The caller folds the
    /// failure it wants - the arming under test - and steps the sweep.
    fn nudge_seat(
        dir: &tempfile::TempDir,
        label: &str,
    ) -> (Arc<Workspace>, SessionSlot, tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>) {
        let (ws, rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let key = SessionSlot::from_str_for_test(label);
        ws.mark_session_connected_for_test(&key, label);
        ws.enable_test_dispatch_intercept();
        (ws, key, rx)
    }

    /// Fold one agent event into `key`'s domain the way a session task
    /// would, so these tests drive the real arming rather than hand-set it.
    fn fold_event(ws: &Arc<Workspace>, key: &SessionSlot, event: &forge_agent::client::AgentEvent) {
        let domain = ws.domain_session_for(key).expect("the seat's session");
        let mut guard = domain.lock();
        crate::session_task::apply_event_to_domain(&mut guard, event);
    }

    /// An errored `Result` carrying the CLI's own errors.
    fn failed_result(errors: &[&str]) -> forge_agent::client::AgentEvent {
        forge_agent::client::AgentEvent::SdkMessage {
            session_id: "nudge".to_owned(),
            msg: serde_json::from_value(serde_json::json!({
                "type": "result",
                "subtype": "error_during_execution",
                "duration_ms": 1,
                "duration_api_ms": 1,
                "is_error": true,
                "num_turns": 1,
                "session_id": "nudge",
                "errors": errors,
            }))
            .expect("parse result message"),
        }
    }

    /// The wire `api_retry` frame, where a classification reaches the core.
    fn retried_with(error: &str, status: u16) -> forge_agent::client::AgentEvent {
        forge_agent::client::AgentEvent::SdkMessage {
            session_id: "nudge".to_owned(),
            msg: serde_json::from_value(serde_json::json!({
                "type": "system",
                "subtype": "api_retry",
                "session_id": "nudge",
                "attempt": 1,
                "max_retries": 4,
                "retry_delay_ms": 500,
                "error_status": status,
                "error": error,
            }))
            .expect("parse api_retry message"),
        }
    }

    /// The prompts `ws` captured, in order.
    fn dispatched_prompts(ws: &Arc<Workspace>) -> Vec<String> {
        ws.drain_test_dispatch_buffer()
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::PromptUnder { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    /// Step the clock past the seat's delay and sweep, the way the
    /// scheduler's next tick would - `now` is injected so these tests stay
    /// deterministic rather than waiting on a clock.
    fn sweep_after_the_delay(ws: &Arc<Workspace>) {
        ws.fire_due_auto_continues(SystemTime::now() + Duration::from_secs(60));
    }

    /// The want #1841 is about: a failed turn nobody is watching gets one
    /// prompt of forge's own - the issue's exact words, through the
    /// dispatched-prompt path - and the frame draws in every view.
    #[test]
    fn a_failed_unwatched_turn_is_nudged_after_its_delay() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, mut rx) = nudge_seat(&dir, "nudge");
        fold_event(&ws, &key, &failed_result(&["API Error: 400 ..."]));

        sweep_after_the_delay(&ws);

        assert_eq!(
            dispatched_prompts(&ws),
            ["The previous turn failed: API Error: 400 .... Continue from where you left off."],
            "one prompt, in the words the issue fixed",
        );
        let drawn = drain_updates(&mut rx);
        assert!(
            drawn.iter().any(|u| matches!(
                u,
                SessionUpdate::ChatAppended { msg: Message::User { .. }, .. }
            )),
            "the nudge draws as the turn the model received: {drawn:?}",
        );
    }

    /// Condition (a): a turn the reader cancelled is the reader's own act.
    /// It arms nothing, so nothing fires however far the clock is stepped.
    #[test]
    fn a_cancelled_turn_never_fires() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, _rx) = nudge_seat(&dir, "cancelled");
        ws.domain_session_for(&key).expect("session").lock().pending_cancel = true;

        fold_event(&ws, &key, &failed_result(&["aborted_streaming"]));
        assert!(
            ws.domain_session_for(&key).expect("session").lock().auto_continue.is_none(),
            "the fold refuses to arm a cancelled turn",
        );
        ws.domain_session_for(&key).expect("session").lock().failed_turn_at =
            Some(SystemTime::now() - Duration::from_secs(60));
        sweep_after_the_delay(&ws);

        assert!(
            dispatched_prompts(&ws).is_empty(),
            "a mark with no armed nudge is not something the sweep fires",
        );
    }

    /// The reader still has the delay to look. Nothing goes out early.
    #[test]
    fn nothing_fires_before_the_delay_elapses() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, _rx) = nudge_seat(&dir, "early");
        fold_event(&ws, &key, &failed_result(&["API Error: 400 ..."]));

        // The sweep runs now, which is inside the delay the arming set.
        ws.fire_due_auto_continues(SystemTime::now());

        assert!(dispatched_prompts(&ws).is_empty(), "the delay has not run out yet");
    }

    /// Condition (b), #1612's own boundary: a seat the reader opened since
    /// the failure is not nudged - they have seen what failed.
    #[tokio::test]
    async fn a_seat_shown_since_the_failure_never_fires() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, _rx) = nudge_seat(&dir, "opened");
        fold_event(&ws, &key, &failed_result(&["API Error: 400 ..."]));

        ws.hold_seat(&key).await;
        ws.release_seat(&key);
        sweep_after_the_delay(&ws);

        assert!(
            dispatched_prompts(&ws).is_empty(),
            "the reader opened the seat and saw the failure; forge does not prompt over them",
        );
    }

    /// A view holding the seat as it fails is watching the failure happen,
    /// so there is nothing to send it.
    #[tokio::test]
    async fn a_seat_a_view_is_holding_never_fires() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, _rx) = nudge_seat(&dir, "watching");
        ws.hold_seat(&key).await;
        // The hold predates the failure, so only the held check can refuse
        // this one - the shown-since stamp is older than the mark.
        ws.domain_session_for(&key).expect("session").lock().shown_at =
            Some(SystemTime::now() - Duration::from_secs(60));

        fold_event(&ws, &key, &failed_result(&["API Error: 400 ..."]));
        sweep_after_the_delay(&ws);

        assert!(
            dispatched_prompts(&ws).is_empty(),
            "the reader is watching the failure; nothing is sent to the seat under them",
        );
    }

    /// The repeat case, settled: once per unopened failure episode. The
    /// nudge's own continuation failing must not become a prompt loop.
    #[test]
    fn a_second_failure_after_a_fire_does_not_fire_again() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, _rx) = nudge_seat(&dir, "repeat");
        fold_event(&ws, &key, &failed_result(&["API Error: 400 ..."]));
        sweep_after_the_delay(&ws);
        assert_eq!(dispatched_prompts(&ws).len(), 1, "precondition: the failure was nudged");

        fold_event(&ws, &key, &failed_result(&["API Error: 400 ..."]));
        assert!(
            ws.domain_session_for(&key).expect("session").lock().auto_continue.is_none(),
            "the episode has had its nudge; a second failure arms nothing",
        );
        sweep_after_the_delay(&ws);

        assert!(
            dispatched_prompts(&ws).is_empty(),
            "and the sweep sends nothing more for the same unopened failure",
        );
    }

    /// The episode ends when the reader looks: a later failure is a new one
    /// and gets its own nudge.
    #[tokio::test]
    async fn an_opened_seat_reopens_the_nudge_for_a_later_failure() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, _rx) = nudge_seat(&dir, "reopened");
        fold_event(&ws, &key, &failed_result(&["API Error: 400 ..."]));
        sweep_after_the_delay(&ws);
        assert_eq!(dispatched_prompts(&ws).len(), 1, "precondition: the first failure was nudged");

        ws.hold_seat(&key).await;
        ws.release_seat(&key);
        fold_event(&ws, &key, &failed_result(&["API Error: 400 ..."]));
        sweep_after_the_delay(&ws);

        assert_eq!(
            dispatched_prompts(&ws).len(),
            1,
            "the reader looked, so the next failure is a new episode and is nudged",
        );
    }

    /// And the other half of that boundary: a classification that is NOT a
    /// transient server error does not exempt the failure. The terminal
    /// continues server errors only, so a billing or auth death is exactly
    /// the one this nudge exists for - starving it here would leave the seat
    /// sitting, the bug #1841 is about.
    #[test]
    fn a_classification_the_terminal_does_not_continue_still_nudges() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, _rx) = nudge_seat(&dir, "billing");
        fold_event(&ws, &key, &retried_with("billing_error", 402));
        fold_event(&ws, &key, &failed_result(&["billing_error"]));

        sweep_after_the_delay(&ws);

        assert_eq!(
            dispatched_prompts(&ws),
            ["The previous turn failed: billing_error. Continue from where you left off."],
            "a failure the terminal leaves alone is nudged",
        );
    }

    /// The no-double-fire boundary: a transient server error is the
    /// terminal's own dead-turn path. The sweep must not nudge beside it,
    /// and steps the clock all it likes.
    #[test]
    fn a_transient_server_failure_is_left_to_the_terminal() {
        let dir = tempdir().expect("tempdir");
        let (ws, key, _rx) = nudge_seat(&dir, "transient");
        fold_event(&ws, &key, &retried_with("server_error", 529));
        fold_event(&ws, &key, &failed_result(&["server_error"]));
        ws.domain_session_for(&key).expect("session").lock().failed_turn_at =
            Some(SystemTime::now() - Duration::from_secs(60));

        sweep_after_the_delay(&ws);

        assert!(
            dispatched_prompts(&ws).is_empty(),
            "the terminal continues this one; the core must not fire beside it",
        );
    }

    /// The cron delivery path rides the helper: a cron fired into a
    /// mid-turn lead signals `PromptQueuedWhileBusy` on top of the
    /// `CronPromptAppended` echo; an idle fire stays silent.
    #[test]
    fn cron_fired_mid_turn_signals_prompt_queued_while_busy() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("cronlead", "/tmp/cron-lead-queued");
        let cwd = project_expanded_path(&ws, "cronlead");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionSlot::lead("TestOrg", "cronlead");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
        ws.enable_test_dispatch_intercept();

        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, &test_cron("c1", "cronlead", "morning"), false);
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        assert!(
            !drain_updates(&mut rx)
                .iter()
                .any(|u| matches!(u, SessionUpdate::PromptQueuedWhileBusy { .. })),
            "an idle cron fire must not signal PromptQueuedWhileBusy",
        );

        ws.domain_session_for(&lead_key).expect("domain").lock().turn_pending = true;
        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, &test_cron("c2", "cronlead", "again"), false);
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let signalled = drain_updates(&mut rx)
            .into_iter()
            .any(|u| matches!(u, SessionUpdate::PromptQueuedWhileBusy { key: k } if k == lead_key));
        assert!(signalled, "a cron fired mid-turn signals PromptQueuedWhileBusy");
    }

    /// A failed dispatch must not strand a queue signal: the log-only
    /// failure sites (kick, notices, drains) never emit a TurnError,
    /// so a signal sent despite the failure would survive on a live
    /// bucket with nothing to clear it.
    #[test]
    fn failed_dispatch_does_not_signal_queued_while_busy() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let key = SessionSlot::from_str_for_test("doomed-uuid");
        ws.mark_session_connected_for_test(&key, "doomed-uuid");
        ws.domain_session_for(&key).expect("domain").lock().turn_pending = true;

        let result = ws.dispatch_workspace_prompt(&key, "lost".to_owned());
        assert!(result.is_err(), "no SessionTask and no stub conn: the dispatch fails");
        assert!(
            !drain_updates(&mut rx)
                .iter()
                .any(|u| matches!(u, SessionUpdate::PromptQueuedWhileBusy { .. })),
            "a failed dispatch must not signal PromptQueuedWhileBusy",
        );
    }

    /// The gotify running-lead delivery rides the helper too - a
    /// second family (after cron) through a different entry path:
    /// idle fire silent, turn in flight then the signal.
    #[test]
    fn gotify_delivered_mid_turn_signals_prompt_queued_while_busy() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("glead", "/tmp/gotify-lead-queued");
        let cwd = project_expanded_path(&ws, "glead");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionSlot::lead("TestOrg", "glead");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
        ws.enable_test_dispatch_intercept();
        let notif = gotify_notif("Backups", "Nightly backup", "done", 5);

        crate::spawn::deliver_gotify_message(&ws, "glead", None, notif.clone());
        assert!(
            !drain_updates(&mut rx)
                .iter()
                .any(|u| matches!(u, SessionUpdate::PromptQueuedWhileBusy { .. })),
            "an idle gotify fire must not signal PromptQueuedWhileBusy",
        );

        ws.domain_session_for(&lead_key).expect("domain").lock().turn_pending = true;
        crate::spawn::deliver_gotify_message(&ws, "glead", None, notif);
        let signalled = drain_updates(&mut rx)
            .into_iter()
            .any(|u| matches!(u, SessionUpdate::PromptQueuedWhileBusy { key: k } if k == lead_key));
        assert!(signalled, "a gotify delivered mid-turn signals PromptQueuedWhileBusy");
    }

    fn slack_message_for(text: &str) -> forge_primitives::slack::SlackMessage {
        forge_primitives::slack::SlackMessage {
            workspace: "acme".to_owned(),
            conversation: "D1".to_owned(),
            conversation_label: "U9".to_owned(),
            ts: "100.000001".to_owned(),
            thread_ts: None,
            user: Some("U9".to_owned()),
            author: None,
            text: text.to_owned(),
            parent_user_id: None,
            latest_reply: None,
            files: Vec::new(),
        }
    }

    /// The lead-owned subscription's running lead receives the message as
    /// a prompt, and the asleep path never fires.
    #[test]
    fn slack_delivery_to_a_running_lead_dispatches_a_prompt() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut update_rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("glead", "/tmp/slack-lead-running");
        let cwd = project_expanded_path(&ws, "glead");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionSlot::lead("TestOrg", "glead");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
        ws.enable_test_dispatch_intercept();

        crate::spawn::deliver_slack_message(&ws, "glead", None, vec![slack_message_for("ping")]);

        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|cmd| matches!(
                cmd,
                crate::protocol::Command::Prompt { key, .. }
                    | crate::protocol::Command::PromptUnder { key, .. }
                    if *key == lead_key
            )),
            "the running lead receives the message as a prompt: {dispatched:?}",
        );
        assert!(
            dispatched
                .iter()
                .all(|cmd| !matches!(cmd, crate::protocol::Command::SpawnProject { .. })),
            "a running lead must not fire the asleep spawn path",
        );
        assert_eq!(
            ws.parked_by_slot
                .lock()
                .get(&crate::SessionSlot::lead("TestOrg", "glead"))
                .map_or(0, |parked| parked.slack.len()),
            0,
            "a running lead's delivery is dispatched, not parked",
        );

        // The delivery ALSO echoes the block into the lead's chat: the CLI
        // never echoes a stdin-injected prompt, so without it the user sees
        // nothing.
        let echoed = drain_updates(&mut update_rx).into_iter().any(|u| {
            matches!(
                u,
                crate::protocol::SessionUpdate::SlackMessageAppended { key, prose, .. }
                    if key == lead_key
                        && prose.starts_with("[Slack")
                        && prose.contains("ping")
            )
        });
        assert!(echoed, "a running-lead delivery emits a SlackMessageAppended echo with the body");
    }

    /// The lead arm carries the stricter rule - a failed dispatch returns false
    /// so the sweep re-runs the message - and must not have echoed before it.
    /// The pooled lead stays in place so the delivery reaches the lead arm; the
    /// failure is a task channel whose receiver is gone, the teardown window a
    /// session close leaves behind.
    #[test]
    fn a_failed_lead_dispatch_leaves_no_echo_for_the_sweep_to_duplicate() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut update_rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("glead", "/tmp/slack-lead-doomed");
        let lead_key = SessionSlot::lead("TestOrg", "glead");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
        // A sender whose receiver is dropped: `dispatch` takes the production
        // routing path and returns `SessionClosed`, instead of falling through
        // to the test-only path that would run it against the stub handle.
        let (tx, rx) = mpsc::unbounded_channel::<crate::protocol::Command>();
        drop(rx);
        ws.command_senders.lock().insert(lead_key.clone(), tx);

        let delivered = crate::spawn::deliver_slack_message(
            &ws,
            "glead",
            None,
            vec![slack_message_for("ping")],
        );

        assert!(!delivered, "a failed dispatch tells the sweep to re-run the message");
        let echoed = drain_updates(&mut update_rx)
            .into_iter()
            .any(|u| matches!(u, crate::protocol::SessionUpdate::SlackMessageAppended { .. }));
        assert!(!echoed, "a failed lead dispatch must not echo a SlackMessageAppended");
    }

    /// With no running session, the message parks for the project
    /// lead's slot and the project's spawn is fired to pick it up.
    #[test]
    fn slack_delivery_to_an_asleep_lead_buffers_and_spawns_the_project() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("glead", "/tmp/slack-lead-asleep");
        ws.enable_test_dispatch_intercept();

        crate::spawn::deliver_slack_message(&ws, "glead", None, vec![slack_message_for("wake up")]);

        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|cmd| matches!(
                cmd,
                crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "glead"
            )),
            "the asleep project is spawned: {dispatched:?}",
        );
        assert!(
            dispatched.iter().all(|cmd| !matches!(
                cmd,
                crate::protocol::Command::Prompt { .. }
                    | crate::protocol::Command::PromptUnder { .. }
            )),
            "no session is prompted directly",
        );
        assert_eq!(
            parked_slack_count(&ws, "glead", None),
            1,
            "the message waits for the spawned session to drain",
        );
    }

    /// A sweep re-run of the same message must not double-prompt: the
    /// dedupe drops the second hand-off to the same destination, which is
    /// what makes a post-failure re-sweep idempotent.
    #[test]
    fn a_second_delivery_of_the_same_message_to_the_same_owner_is_dropped() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("glead", "/tmp/slack-double");
        ws.enable_test_dispatch_intercept();

        let message = slack_message_for("hello");
        assert!(
            crate::spawn::deliver_slack_message(&ws, "glead", None, vec![message.clone()]),
            "the first delivery lands",
        );
        assert!(
            crate::spawn::deliver_slack_message(&ws, "glead", None, vec![message]),
            "the re-run reads as already delivered, not as a failure",
        );

        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|cmd| !matches!(
                cmd,
                crate::protocol::Command::Prompt { .. }
                    | crate::protocol::Command::PromptUnder { .. }
            )),
            "the asleep path buffers; the second delivery added no prompt",
        );
        assert_eq!(parked_slack_count(&ws, "glead", None), 1, "one buffered message, not two");
    }

    /// How many Slack messages are parked for `(project, label)`, under the
    /// org the seeded project belongs to.
    fn parked_slack_count(ws: &Workspace, project: &str, label: Option<&str>) -> usize {
        let org =
            ws.list_projects().into_iter().find(|v| v.name == project).expect("seeded project").org;
        ws.parked_by_slot
            .lock()
            .get(&crate::SessionSlot::for_label(&org, project, label))
            .map_or(0, |parked| parked.slack.len())
    }

    fn make_workspace_dir() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// The project declares a model, so a respawn has a canonical name
    /// to stamp over the caller's pin.
    fn make_workspace_dir_declaring_model() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Personal", "OpenRouter-TM"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "deepseek-v4.1-flash"

[[accounts]]
display_name = "Personal"
token = "t"
models = ["claude-opus-5"]
provider = "anthropic"

[[accounts]]
display_name = "OpenRouter-TM"
token = "t"
models = ["deepseek-v4.1-flash"]
provider = "openrouter"
base_url = "https://openrouter.ai/api"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    #[tokio::test]
    async fn new_for_test_opens_redb_under_the_tempdir() {
        let dir = make_workspace_dir();
        let _workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        // The test constructor redirects redb into the config dir's own
        // tempdir, so no test ever opens the real machine store (#392).
        let redirected = dir.path().join("app-support").join("db.redb");
        assert!(redirected.exists(), "new_for_test opens redb under the tempdir app-support base");
    }

    #[tokio::test]
    async fn new_for_test_writes_the_lock_under_the_tempdir() {
        let dir = make_workspace_dir();
        let _workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        // Everything under the app-support base follows the same
        // redirect as redb, so a test run leaves the real directory
        // untouched.
        let base = dir.path().join("app-support");
        assert!(
            base.join("locks").is_dir(),
            "new_for_test takes the single-instance lock under the tempdir base",
        );
    }

    #[tokio::test]
    async fn get_agent_handle_default_is_idempotent() {
        let dir = make_workspace_dir();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_ready_account("Stargate");
        let settings = SessionLaunchSettings::default();

        let handle1 = workspace
            .get_agent_handle(
                SessionTarget::Default,
                settings.clone(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("first");
        let handle2 = workspace
            .get_agent_handle(SessionTarget::Default, settings, &crate::protocol::SpawnRole::Lead)
            .expect("second");

        assert!(Arc::ptr_eq(&handle1, &handle2), "expected pool hit for repeated Default target");
        assert_eq!(workspace.pool.lock().len(), 1);
    }

    #[tokio::test]
    async fn distinct_targets_pool_distinct_entries() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[orgs.projects]]
name = "dotfiles"
path = "~/Projects/dotfiles"
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_ready_account("Stargate");
        let settings = SessionLaunchSettings::default();

        let _ = workspace
            .get_agent_handle(
                SessionTarget::Default,
                settings.clone(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("default");
        let _ = workspace
            .get_agent_handle(
                SessionTarget::Default,
                settings.clone(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("default again");
        assert_eq!(workspace.pool.lock().len(), 1, "Default is idempotent");

        let _ = workspace
            .get_agent_handle(
                SessionTarget::Named("dotfiles".to_owned()),
                settings,
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("named");
        assert_eq!(workspace.pool.lock().len(), 2, "a distinct target adds a pool entry");
    }

    #[tokio::test]
    async fn shutdown_drains_pool() {
        let dir = make_workspace_dir();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_ready_account("Stargate");
        let handle = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("default");

        // Pool has one entry going in.
        assert_eq!(workspace.pool.lock().len(), 1);

        // Shutdown consumes self and must return. Drops `command_senders`,
        // which closes each `SessionTask`'s command channel; the spawned
        // task then exits and drops its `handle` clone.
        workspace.shutdown();

        // The spawned `SessionTask` exits asynchronously after its
        // command channel closes; yield to let it run to completion
        // so the final `handle` drop is observable in `strong_count`.
        for _ in 0..16 {
            tokio::task::yield_now().await;
            if Arc::strong_count(&handle) == 1 {
                break;
            }
        }
        assert_eq!(Arc::strong_count(&handle), 1);
    }

    #[tokio::test]
    async fn get_agent_handle_named_project_resolves() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[orgs.projects]]
name = "dotfiles"
path = "~/Projects/dotfiles"
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");

        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_ready_account("Stargate");
        let _ = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("default");
        let _ = workspace
            .get_agent_handle(
                SessionTarget::Named("dotfiles".to_owned()),
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("named");
        assert_eq!(workspace.pool.lock().len(), 2);
    }

    #[tokio::test]
    async fn get_agent_handle_named_unknown_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");

        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let result = workspace.get_agent_handle(
            SessionTarget::Named("nonexistent".to_owned()),
            SessionLaunchSettings::default(),
            &crate::protocol::SpawnRole::Lead,
        );
        let Err(err) = result else { panic!("unknown project name should error") };
        let err_string = format!("{err}");
        assert!(
            err_string.contains("nonexistent"),
            "error should mention the project name; got: {err_string}"
        );
    }

    fn make_workspace_dir_with_two_accounts() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate", "Gateway"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Gateway"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// The migration's other half: a boot has to run the copy, or the
    /// table stays empty and every persisted worker starts fresh on
    /// every boot.
    #[tokio::test]
    async fn booting_copies_the_persisted_workers_into_the_sessions_table() {
        let dir = make_workspace_dir_with_two_accounts();
        let app_support = dir.path().join("app-support");
        fs::create_dir_all(&app_support).expect("app-support dir");
        let db = crate::store::Db::open(&app_support.join("db.redb")).expect("open db");
        // The key the boot derives, so the seeded row is one it can match:
        // the config resolves the project path, so a literal from the
        // fixture is not the same string.
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        let project_key = forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            &config.projects[0].path.to_string_lossy(),
        ));
        crate::store::dynamic_workers::insert_for_test(
            &db,
            &crate::store::dynamic_workers::DynamicWorker {
                project_key,
                label: "steward".to_owned(),
                charter: "mind the queues".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
        )
        .expect("seed the worker a previous build persisted");
        drop(db);

        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("new");
        let db = workspace.db.lock();
        let row =
            crate::store::sessions::get(db.as_ref().expect("db"), "Default", "forge", "steward")
                .expect("read")
                .expect("the boot moved the persisted worker over");
        assert_eq!(row.label, "steward");
        assert_eq!(row.session_id, None, "its id is derived when the row is read");
    }

    /// The ordinary case the merge exists for: `sessions` already holds a
    /// row - every lead spawn writes one - while a worker's spawn args
    /// live only in the retired table, because the release before this one
    /// wrote the worker's id to `sessions` and its args to
    /// `dynamic_workers`. A sweep gated on "the sessions table is empty"
    /// skips this store entirely and then drops the only copy of the
    /// args, so the worker comes back on the next boot with an empty
    /// charter.
    ///
    /// The steward's own row is there too, because that is what the
    /// released build leaves: `record_session_id` wrote the worker's
    /// occupant id to `sessions` under the same `(org, project, label)`
    /// the retired table keys its args by. Every worker live at the
    /// moment of upgrade therefore arrives at the merge arm, and the id
    /// on that row is the thing it must not lose.
    #[tokio::test]
    async fn booting_merges_worker_args_when_the_sessions_table_already_has_rows() {
        let dir = make_workspace_dir_with_two_accounts();
        let app_support = dir.path().join("app-support");
        fs::create_dir_all(&app_support).expect("app-support dir");
        let db = crate::store::Db::open(&app_support.join("db.redb")).expect("open db");
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        let project_key = forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            &config.projects[0].path.to_string_lossy(),
        ));
        crate::store::sessions::put(
            &db,
            &crate::store::sessions::SessionRecord {
                org: "Default".to_owned(),
                project: "forge".to_owned(),
                label: forge_primitives::LEAD_LABEL.to_owned(),
                session_id: Some("lead-session-id".to_owned()),
                charter: None,
                kick: None,
                resume_kick: None,
                interactive: None,
                is_git_repo: None,
                mcp_families: None,
            },
        )
        .expect("seed the lead row a spawn writes");
        crate::store::sessions::put(
            &db,
            &crate::store::sessions::SessionRecord {
                org: "Default".to_owned(),
                project: "forge".to_owned(),
                label: "steward".to_owned(),
                session_id: Some("steward-id".to_owned()),
                charter: None,
                kick: None,
                resume_kick: None,
                interactive: None,
                is_git_repo: None,
                mcp_families: None,
            },
        )
        .expect("seed the steward's own row, as the released build leaves it");
        crate::store::dynamic_workers::insert_for_test(
            &db,
            &crate::store::dynamic_workers::DynamicWorker {
                project_key,
                label: "steward".to_owned(),
                charter: "mind the queues".to_owned(),
                kick: Some("begin".to_owned()),
                resume_kick: Some("re-read the notes".to_owned()),
                interactive: true,
            },
        )
        .expect("seed the worker only the retired table describes");
        drop(db);

        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("new");

        let guard = workspace.db.lock();
        let db = guard.as_ref().expect("db");
        let steward = crate::store::sessions::get(db, "Default", "forge", "steward")
            .expect("read")
            .expect("the sweep runs even though the lead row is there");
        assert_eq!(
            steward.charter.as_deref(),
            Some("mind the queues"),
            "and carries the spawn args over from the retired table",
        );
        assert_eq!(steward.kick.as_deref(), Some("begin"));
        assert_eq!(steward.resume_kick.as_deref(), Some("re-read the notes"));
        assert_eq!(
            steward.session_id.as_deref(),
            Some("steward-id"),
            "onto the row it already had, keeping the occupant that row names - a plain write \
             here would blank the id, re-minting one and orphaning the transcript",
        );
        assert_eq!(
            crate::store::dynamic_workers::count(db).expect("count"),
            0,
            "the retired table is drained",
        );
        // The drop itself, which the count cannot show: an emptied table
        // and a dropped one both report zero rows, so ask whether there is
        // still a table to drop.
        assert!(
            !crate::store::dynamic_workers::drop_table(db).expect("drop"),
            "and the boot dropped it, rather than leaving an empty table behind",
        );
        assert_eq!(steward.interactive, Some(true));
        let lead =
            crate::store::sessions::get(db, "Default", "forge", forge_primitives::LEAD_LABEL)
                .expect("read")
                .expect("the row that was already there survives");
        assert_eq!(lead.session_id.as_deref(), Some("lead-session-id"), "with its id untouched");
    }

    /// The retired table is dropped only once every row it held has been
    /// copied. A row no configured project can key stays where it is, so
    /// the table has to stay with it: dropped, that worker exists nowhere
    /// else and its label silently stops re-spawning, with nothing on
    /// screen to say which one went.
    #[tokio::test]
    async fn booting_keeps_the_retired_table_when_a_row_could_not_be_keyed() {
        let dir = make_workspace_dir_with_two_accounts();
        let app_support = dir.path().join("app-support");
        fs::create_dir_all(&app_support).expect("app-support dir");
        let db = crate::store::Db::open(&app_support.join("db.redb")).expect("open db");
        crate::store::dynamic_workers::insert_for_test(
            &db,
            &crate::store::dynamic_workers::DynamicWorker {
                project_key: "a-project-the-config-no-longer-names".to_owned(),
                label: "steward".to_owned(),
                charter: "mind the queues".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
        )
        .expect("seed a worker whose project is gone");
        drop(db);

        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("new");

        let guard = workspace.db.lock();
        let left = crate::store::dynamic_workers::list_all(guard.as_ref().expect("db"))
            .expect("read the retired table");
        assert_eq!(
            left.len(),
            1,
            "the row could not be keyed, so it is still only there and the table must survive",
        );
        assert_eq!(left[0].label, "steward");
    }

    /// A project with no lead anywhere spawns under an id forge mints,
    /// and the row holds it: the second start re-enters that session
    /// instead of minting a third id for a project that had none.
    #[tokio::test]
    async fn a_lead_spawn_mints_an_id_and_records_it() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.gateway_ready.store(true, std::sync::atomic::Ordering::Release);
        workspace.seed_test_ready_account("Stargate");

        let _handle = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("spawn");

        let id = workspace
            .stored_session_id("Default", "forge", forge_primitives::LEAD_LABEL)
            .expect("the store is readable")
            .expect("the lead's id reaches the store");
        assert!(
            uuid::Uuid::parse_str(&id).is_ok(),
            "the id is a uuid, which is what the catalog's validators accept: {id}",
        );
        assert_eq!(
            workspace
                .stored_resume_id(&SessionSlot::lead("Default", "forge"), false)
                .expect("the store is readable"),
            Some(id.clone()),
            "a later start re-enters the recorded session",
        );
    }

    /// The row outlives the process: a second workspace on the same store
    /// reads the id the first one recorded rather than starting another
    /// session under a new one.
    #[tokio::test]
    async fn a_lead_row_survives_a_restart() {
        let dir = make_workspace_dir_with_two_accounts();
        let id = {
            let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
            workspace.gateway_ready.store(true, std::sync::atomic::Ordering::Release);
            workspace.seed_test_ready_account("Stargate");
            let _handle = workspace
                .get_agent_handle(
                    SessionTarget::Default,
                    SessionLaunchSettings::default(),
                    &crate::protocol::SpawnRole::Lead,
                )
                .expect("spawn");
            workspace
                .stored_session_id("Default", "forge", forge_primitives::LEAD_LABEL)
                .expect("the store is readable")
                .expect("the lead's id reaches the store")
        };

        let restarted = Workspace::new_for_test(dir.path().to_owned()).expect("second boot");
        let project = restarted.config.default_project().clone();
        assert_eq!(
            restarted
                .stored_resume_id(&SessionSlot::lead(&project.org, &project.name), false)
                .expect("the store is readable"),
            Some(id.clone()),
            "the second boot re-enters the session the first one recorded",
        );
    }

    /// `--new` rewrites the lead's row rather than reusing what it holds:
    /// the flag is what makes the boot wave's leads come up fresh, and a
    /// `force_new` that fell through to the store would resume the very
    /// session it was given to replace.
    #[tokio::test]
    async fn force_new_rewrites_the_lead_row_rather_than_reusing_it() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.gateway_ready.store(true, std::sync::atomic::Ordering::Release);
        workspace.seed_test_ready_account("Stargate");
        let project = workspace.config.default_project().clone();
        workspace.record_session_id(
            &project.org,
            &project.name,
            forge_primitives::LEAD_LABEL,
            "stored-lead-id",
        );

        let _handle = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings { force_new: true, ..SessionLaunchSettings::default() },
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("spawn");

        let id = workspace
            .stored_session_id(&project.org, &project.name, forge_primitives::LEAD_LABEL)
            .expect("the store is readable")
            .expect("the row still holds an id");
        assert_ne!(id, "stored-lead-id", "`--new` must not reuse the id the store held");
        assert!(
            uuid::Uuid::parse_str(&id).is_ok(),
            "the row is rewritten with the id this spawn minted: {id}",
        );
    }

    /// A row with no id, as the first boot after the store gained ids
    /// leaves it.
    fn seed_session_row(workspace: &Workspace, org: &str, project: &str, label: &str) {
        let db = workspace.db.lock();
        let db = db.as_ref().expect("db");
        crate::store::sessions::put(
            db,
            &crate::store::sessions::SessionRecord {
                org: org.to_owned(),
                project: project.to_owned(),
                label: label.to_owned(),
                session_id: None,
                charter: None,
                kick: None,
                resume_kick: None,
                interactive: None,
                is_git_repo: None,
                mcp_families: None,
            },
        )
        .expect("seed the row");
    }

    fn project_key_for(project: &LoadedProject) -> crate::target::ProjectKey {
        ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            &project.path.to_string_lossy(),
        )))
    }

    /// The store is the only source for a lead's resume. A row with no id
    /// is a session that has never run, so the spawn mints a fresh one -
    /// even with a lead transcript sitting in the catalog, which is what
    /// every boot read before the store held ids.
    #[tokio::test]
    async fn a_lead_row_with_no_id_starts_fresh() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let project = workspace.config.default_project().clone();
        workspace.catalog.lock().insert(
            project_key_for(&project),
            vec![forge_primitives::SDKSessionInfo {
                session_id: "catalog-lead-id".to_owned(),
                summary: "lead".to_owned(),
                last_modified: 0,
                file_size: None,
                custom_title: None,
                first_prompt: None,
                git_branch: None,
                cwd: None,
                storage_key: String::new(),
                tag: None,
                created_at: None,
            }],
        );
        seed_session_row(&workspace, &project.org, &project.name, forge_primitives::LEAD_LABEL);

        assert_eq!(
            workspace
                .stored_resume_id(&SessionSlot::lead(&project.org, &project.name), false)
                .expect("the store is readable"),
            None,
            "a row with no id has nothing to resume, whatever the catalog holds",
        );
    }

    /// A row that cannot be read is not a row without an id. Minting on a
    /// read error would fork the session and write the new id over the
    /// row it failed to read, so the spawn is refused instead.
    #[tokio::test]
    async fn an_unreadable_session_row_refuses_the_spawn_rather_than_forking() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let project = workspace.config.default_project().clone();
        {
            let db = workspace.db.lock();
            crate::store::sessions::put_raw_for_test(
                db.as_ref().expect("test store"),
                &project.org,
                &project.name,
                forge_primitives::LEAD_LABEL,
                b"not a session record",
            )
            .expect("plant the undecodable row");
        }

        assert!(
            matches!(
                workspace.stored_resume_id(&SessionSlot::lead(&project.org, &project.name), false),
                Err(crate::error::WorkspaceError::SessionStoreUnreadable { .. })
            ),
            "a row that cannot be read is refused, not treated as absent",
        );
        assert!(
            workspace.resolve_slot(&SessionTarget::Named(project.name.clone())).is_err()
                || workspace
                    .stored_resume_id(&SessionSlot::lead(&project.org, &project.name), false)
                    .is_err(),
            "and the refusal reaches the spawn's target resolution, so nothing mints over it",
        );
        assert!(
            crate::store::sessions::get(
                workspace.db.lock().as_ref().expect("test store"),
                &project.org,
                &project.name,
                forge_primitives::LEAD_LABEL,
            )
            .is_err(),
            "the refused spawn left the unreadable row exactly as it found it",
        );
    }

    /// The store follows the id the CLI is actually running. An
    /// in-session `/resume`, a `/clear`, a login or a logout move a
    /// session's id without forge choosing it, and a boot resolves a
    /// session from this row: without this the next boot resumes the
    /// stale id and the registration keeps naming a session that is no
    /// longer running.
    #[tokio::test]
    async fn the_store_follows_the_id_the_cli_adopted() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let key = SessionSlot::lead("Default", "forge");
        workspace.seed_test_bound_session(&key, "Stargate");
        seed_session_row(&workspace, "Default", "forge", forge_primitives::LEAD_LABEL);

        workspace.note_running_session_id(&key, "adopted-id");
        assert_eq!(
            workspace
                .stored_session_id("Default", "forge", forge_primitives::LEAD_LABEL)
                .expect("the store is readable")
                .as_deref(),
            Some("adopted-id"),
            "the row holds the id the CLI adopted, not a stale one",
        );

        workspace.note_running_session_id(&key, "adopted-id");
        assert_eq!(
            workspace
                .stored_session_id("Default", "forge", forge_primitives::LEAD_LABEL)
                .expect("the store is readable")
                .as_deref(),
            Some("adopted-id"),
            "and re-noting the same id leaves it alone",
        );
    }

    #[tokio::test]
    async fn a_fresh_spawn_stamps_the_gateway_base_url_and_dummy_credential() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        // The listener's own tests cover the socket; this one pins what
        // the child is stamped with, so the boot gate is opened by hand.
        workspace.gateway_ready.store(true, std::sync::atomic::Ordering::Release);
        workspace.seed_test_ready_account("Stargate");

        let handle = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("spawn");
        let env = handle.env();
        let base = env
            .get("ANTHROPIC_BASE_URL")
            .expect("the gateway stamps a base URL on every registered spawn");
        assert!(
            base.starts_with("http://127.0.0.1:") && base.contains("/Default/forge/"),
            "the base URL names the listener and the three routing segments: {base}",
        );
        assert_eq!(
            env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
            Some(forge_gateway::binding::DUMMY_CREDENTIAL),
            "the child's credential variable carries the dummy, not the real token",
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some(""),
            "an API-key source is forced empty so the CLI cannot ship a foreign credential",
        );

        // The binding is keyed by the session's own id - the third
        // routing segment, and the tail of the base URL above - not by
        // the slot, which is the routing key rather than the occupant.
        let session_key = workspace.resolve_slot(&SessionTarget::Default).expect("resolves");
        let session_id = {
            let pool = workspace.pool.lock();
            pool.get(&session_key).expect("the spawn pooled the lead").session_id.clone()
        };
        assert_eq!(
            workspace.gateway.bindings.binding_for("Default", "forge", &session_id),
            Some(AccountKey("Stargate".to_owned())),
            "the spawn registered the session with the gateway",
        );
    }

    #[tokio::test]
    async fn a_project_env_cannot_unstamp_the_gateway_base_url() {
        let dir = make_workspace_dir_with_two_accounts();
        // The project layer carries a base-url key pointing elsewhere:
        // exactly the silent-bypass hazard the stamp closes.
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate", "Gateway"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[orgs.projects.env]
ANTHROPIC_BASE_URL = "http://169.254.10.10:9999"
CLAUDE_CODE_API_BASE_URL = "http://169.254.10.10:9999"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Gateway"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.gateway_ready.store(true, std::sync::atomic::Ordering::Release);
        workspace.seed_test_ready_account("Stargate");

        let handle = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("spawn");
        let env = handle.env();
        let base = env.get("ANTHROPIC_BASE_URL").expect("base url stamped");
        assert!(
            base.starts_with("http://127.0.0.1:") && base.contains("/Default/forge/"),
            "the project layer must not point the child away from the listener: {base}",
        );
        assert_eq!(
            env.get("CLAUDE_CODE_API_BASE_URL").map(String::as_str),
            Some(base.as_str()),
            "the alt base-url variable is stamped to the listener too",
        );
        assert!(
            !env.values().any(|v| v.contains("169.254.10.10")),
            "the foreign host never reaches the child: {env:?}",
        );
    }

    #[tokio::test]
    async fn a_closed_boot_gate_refuses_spawns_naming_the_port() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.gateway_ready.store(false, std::sync::atomic::Ordering::Release);

        let message = match workspace.get_agent_handle(
            SessionTarget::Default,
            SessionLaunchSettings::default(),
            &crate::protocol::SpawnRole::Lead,
        ) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a shut boot gate refuses spawns"),
        };
        assert!(
            message.contains("8787") || message.contains("port"),
            "the refusal names the port, got: {message}",
        );
        assert!(
            message.contains("not ready") || message.contains("boot gate"),
            "the refusal says the gateway is the cause, got: {message}",
        );
    }

    #[tokio::test]
    async fn pool_records_picked_account() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_ready_account("Stargate");
        workspace.seed_test_ready_account("Gateway");
        let _ = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("default");
        let bound = workspace.pool.lock().values().map(|p| p.account.0.clone()).collect::<Vec<_>>();
        assert_eq!(bound.len(), 1);
        // Every spawn walks, so the pick is the first ready account in
        // the org's own order - there is no per-spawn spread.
        assert_eq!(bound[0], "Stargate");
    }

    #[tokio::test]
    async fn project_account_pin_excludes_unpinned_account() {
        // Three accounts globally, all Ready; the default org pins only
        // {Stargate, Gateway}, so a spawn under the default project
        // walks the pinned pair in its own order and never reaches
        // Personal.
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate", "Gateway"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Gateway"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Personal"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");

        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        for account in ["Stargate", "Gateway", "Personal"] {
            workspace.seed_test_ready_account(account);
        }
        let _ = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("default spawn");

        let bound = workspace.pool.lock().values().map(|p| p.account.0.clone()).collect::<Vec<_>>();
        assert_eq!(bound.len(), 1);
        assert_eq!(
            bound[0], "Stargate",
            "the walk takes the first pinned account; Personal is never reached",
        );
    }

    // ---- Refresh + facade tests ----
    //
    // Use a synthetically-registered `DomainSession` so the test
    // doesn't need a real subprocess. The stub handle's command
    // dispatcher captures whatever the workspace pushes through it,
    // which is what we assert on.

    /// Wire one `DomainSession` for `key` against a fresh testing
    /// stub. Returns the workspace, the matching primitives command
    /// receiver, and the registered key.
    fn ws_with_stub_session()
    -> (Arc<Workspace>, mpsc::UnboundedReceiver<forge_primitives::AgentCommand>, SessionSlot) {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("refresh-test");
        let rx = workspace.install_testing_stub(&key);
        // Stamp a session_id so refresh paths don't bail on
        // "no session_id yet".
        if let Some(domain) = workspace.domain_session_for(&key) {
            domain.lock().session_id = Some(forge_primitives::SessionId::new(key.display()));
        }
        (workspace, rx, key)
    }

    #[test]
    fn refresh_status_snapshot_dispatches_get_status_snapshot() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.refresh_status_snapshot(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::AgentCommand::GetStatusSnapshot { .. }));
    }

    #[test]
    fn refresh_context_usage_dispatches_get_context_usage() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.refresh_context_usage(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::AgentCommand::GetContextUsage { .. }));
    }

    #[test]
    fn refresh_oauth_credentials_dispatches() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.refresh_oauth_credentials_snapshot(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::AgentCommand::GetOauthCredentialsSnapshot { .. }));
    }

    #[test]
    fn reload_plugins_dispatches() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.reload_plugins(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::AgentCommand::ReloadPlugins { .. }));
    }

    #[test]
    fn refresh_mcp_snapshot_dispatches() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.refresh_mcp_snapshot(&key).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::AgentCommand::GetMcpSnapshot { .. }));
    }

    /// The OS walk's answer lands on the session that produced it, and a
    /// second session's read does not see it: the walk describes one
    /// subprocess tree, so a read that answered another slot's would
    /// report a process tree that is not running there.
    #[test]
    fn a_stored_process_snapshot_answers_only_its_own_session() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let walked = key_with_stub_handle(&workspace, "walked");
        let other = key_with_stub_handle(&workspace, "other");
        let walked_at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(60);

        workspace.store_process_snapshot(&walked, Some(snapshot_walked_at(walked_at)));

        assert_eq!(
            workspace.process_snapshot(&walked).map(|snapshot| snapshot.scanned_at),
            Some(walked_at),
            "the session that was walked reads its own snapshot back",
        );
        assert!(
            workspace.process_snapshot(&other).is_none(),
            "and its neighbour reads none, rather than the walk of another tree",
        );
    }

    /// The walk described a subprocess tree that is gone, so the read
    /// must not keep serving it.
    #[test]
    fn a_cleared_process_snapshot_reads_as_none() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = key_with_stub_handle(&workspace, "walked");
        workspace.store_process_snapshot(
            &key,
            Some(snapshot_walked_at(std::time::SystemTime::UNIX_EPOCH)),
        );

        workspace.store_process_snapshot(&key, None);

        assert!(workspace.process_snapshot(&key).is_none(), "a cleared snapshot reads as none");
    }

    /// A slot with no session at all answers rather than panicking, and
    /// holds nothing: a store that minted a domain for whoever asked
    /// would leave the workspace routing to a session nobody runs.
    #[test]
    fn a_process_snapshot_for_an_unregistered_slot_reads_as_none() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let stranger = SessionSlot::from_str_for_test("never-registered");

        workspace.store_process_snapshot(
            &stranger,
            Some(snapshot_walked_at(std::time::SystemTime::UNIX_EPOCH)),
        );

        assert!(
            workspace.process_snapshot(&stranger).is_none(),
            "an unregistered slot holds nothing rather than creating one",
        );
    }

    fn snapshot_walked_at(
        scanned_at: std::time::SystemTime,
    ) -> forge_agent::env::processes::ProcessSnapshot {
        forge_agent::env::processes::ProcessSnapshot { processes: Vec::new(), scanned_at }
    }

    fn key_with_stub_handle(workspace: &Arc<Workspace>, label: &str) -> SessionSlot {
        let key = SessionSlot::from_str_for_test(label);
        drop(workspace.install_testing_stub(&key));
        key
    }

    #[test]
    fn refresh_status_snapshot_unknown_session_errors() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("never-registered");
        let err = workspace.refresh_status_snapshot(&key).expect_err("unknown session");
        assert!(matches!(err, DispatchError::UnknownSession(_)));
    }

    /// `dispatch(Command::Cancel)` falls back to synchronous direct
    /// dispatch when no SessionTask is registered. This is the path
    /// TUI unit tests rely on - `set_active_conn` installs a stub
    /// handle but never spawns a task, and tests need to observe
    /// the primitive command on the rx.
    #[test]
    fn dispatch_falls_back_to_direct_when_no_session_task() {
        let (workspace, mut rx, key) = ws_with_stub_session();
        workspace.dispatch(Command::Cancel { key }).expect("dispatch");
        let cmd = rx.try_recv().expect("queued");
        assert!(matches!(cmd, forge_primitives::AgentCommand::Cancel { .. }));
    }

    /// The two facts the failed-turn mark reads from the routing seam, on
    /// the production path a registered sender puts a dispatch on: Cancel
    /// arms the exempt stamp only while a turn is in flight, and a
    /// committed prompt spends the previous failure.
    #[test]
    fn routing_arms_cancels_in_flight_and_spends_the_failure_mark_on_a_prompt() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("routing-seam");
        workspace.register_domain_session(key.clone(), None);
        // Registers the sender, so dispatch takes the routed path rather
        // than the test-only synchronous fallback.
        workspace.mark_test_session_live(&key);
        let domain = workspace.domain_session_for(&key).expect("registered domain");
        domain.lock().session_id = Some(forge_primitives::SessionId::new(key.display()));

        // An idle Cancel arms nothing: there is no turn to exempt.
        workspace.dispatch(Command::Cancel { key: key.clone() }).expect("dispatch");
        assert!(!domain.lock().pending_cancel, "an idle cancel arms nothing");

        domain.lock().failed_turn_at = Some(std::time::SystemTime::now());
        domain.lock().turn_pending = true;
        workspace.dispatch(Command::Cancel { key: key.clone() }).expect("dispatch");
        assert!(domain.lock().pending_cancel, "a cancel over a live turn arms the stamp");
        assert!(
            workspace.session_failed_turn(&key).is_some(),
            "and leaves the standing mark in place",
        );

        workspace
            .dispatch(Command::Prompt {
                key: key.clone(),
                text: "go".to_owned(),
                attachments: Vec::new(),
            })
            .expect("dispatch");
        assert!(
            workspace.session_failed_turn(&key).is_none(),
            "a committed prompt moves past the failure: the newest turn is this one",
        );
        assert!(
            domain.lock().pending_cancel,
            "a prompt that only queues behind the busy turn keeps the cancel stamp - \
             its error Result still has to read as the reader's own cancel",
        );

        // The busy turn ends, and a prompt committed with nothing in flight
        // starts a new one: the stamp expires there, or this turn's genuine
        // failure would go unmarked.
        domain.lock().turn_pending = false;
        domain.lock().pending_cancel = true;
        workspace
            .dispatch(Command::Prompt {
                key: key.clone(),
                text: "go".to_owned(),
                attachments: Vec::new(),
            })
            .expect("dispatch");
        assert!(
            !domain.lock().pending_cancel,
            "a prompt committed with no turn in flight expires the stamp",
        );
    }

    /// A prompt that only QUEUES behind a live turn does not clear the
    /// classification that describes it: the turn it belongs to has not
    /// ended, and its own transient failure must still read as the
    /// terminal's own. Clearing here would starve the nudge for a
    /// `server_error` the terminal continues, which is the double-fire
    /// boundary crossed the other way.
    #[test]
    fn a_queued_prompt_keeps_the_running_turns_classification() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("queued-classification");
        workspace.register_domain_session(key.clone(), None);
        // Registers the sender, so dispatch takes the routed path rather
        // than the test-only synchronous fallback.
        workspace.mark_test_session_live(&key);
        let domain = workspace.domain_session_for(&key).expect("registered domain");
        domain.lock().session_id = Some(forge_primitives::SessionId::new(key.display()));
        domain.lock().turn_pending = true;
        domain.lock().last_api_retry =
            Some((forge_primitives::ApiRetryError::ServerError, Some(529)));

        workspace
            .dispatch(Command::Prompt {
                key: key.clone(),
                text: "later".to_owned(),
                attachments: Vec::new(),
            })
            .expect("dispatch");

        assert_eq!(
            domain.lock().last_api_retry,
            Some((forge_primitives::ApiRetryError::ServerError, Some(529))),
            "the running turn's classification survives a prompt that only queues",
        );
    }

    /// A committed prompt is the newest turn, so it spends the pending nudge
    /// left by the failure before it - and the classification that nudge was
    /// read against, which left standing would exempt the new turn's own
    /// failure from the arming.
    #[test]
    fn routing_drops_the_pending_nudge_and_its_classification_on_a_prompt() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("routing-nudge");
        workspace.register_domain_session(key.clone(), None);
        // Registers the sender, so dispatch takes the routed path rather
        // than the test-only synchronous fallback.
        workspace.mark_test_session_live(&key);
        let domain = workspace.domain_session_for(&key).expect("registered domain");
        domain.lock().session_id = Some(forge_primitives::SessionId::new(key.display()));
        domain.lock().last_api_retry =
            Some((forge_primitives::ApiRetryError::ServerError, Some(529)));
        domain.lock().auto_continue = Some(crate::domain_session::PendingAutoContinue {
            due_at: std::time::SystemTime::now() + Duration::from_secs(5),
            reason: "server_error".to_owned(),
        });

        workspace
            .dispatch(Command::Prompt {
                key: key.clone(),
                text: "go".to_owned(),
                attachments: Vec::new(),
            })
            .expect("dispatch");

        assert!(
            domain.lock().auto_continue.is_none(),
            "a committed prompt is the newest turn, so the nudge for the old one goes",
        );
        assert!(
            domain.lock().last_api_retry.is_none(),
            "and the old turn's classification cannot exempt the new turn's failure",
        );
    }

    /// The review/close store writes route through the command
    /// bus: a `SaveReviewThreads` dispatch lands in the redb store
    /// (observable via the query-side load), and an `UpsertReviewThread`
    /// dispatch carries its confirmation back on the responder - the
    /// overlay's at-risk flag depends on it.
    #[test]
    fn dispatch_buses_the_review_and_spinner_writes() {
        use forge_primitives::review::{ReviewAnchor, ReviewSide, ReviewStatus, ReviewThread};
        let (workspace, _update_rx) = Workspace::testing_stub();
        let db_dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&db_dir.path().join("db.redb")).expect("db"),
        );
        let thread = ReviewThread {
            id: "t1".to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 1,
                content_hash: 1,
                context: vec!["ctx".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: Vec::new(),
            status: ReviewStatus::Open,
            created_at: "t".to_owned(),
            updated_at: "t".to_owned(),
            commit: None,
        };

        workspace
            .dispatch(Command::SaveReviewThreads {
                project: "forge".to_owned(),
                branch: "feat".to_owned(),
                threads: vec![thread.clone()],
            })
            .expect("dispatch");
        let loaded = workspace.load_review_threads("forge", "feat").expect("load");
        assert_eq!(loaded.len(), 1, "the bus-routed save landed in the store");

        let (respond_tx, mut respond_rx) = tokio::sync::oneshot::channel();
        workspace
            .dispatch(Command::UpsertReviewThread {
                project: "forge".to_owned(),
                branch: "feat".to_owned(),
                thread: thread.clone(),
                respond: Some(respond_tx),
            })
            .expect("dispatch");
        assert!(
            respond_rx.try_recv().expect("response present"),
            "an open store confirms the upsert on the responder"
        );
    }

    /// `/new` and `/resume` re-spawn on the already-pooled handle, where
    /// the spawn-path stamp in `get_agent_handle_at_key` never
    /// runs - the launch settings must pick the pooled session's mode up
    /// at dispatch instead.
    #[test]
    fn respawn_commands_on_a_pooled_session_carry_its_mode() {
        use forge_primitives::permission::PermissionMode;
        let (workspace, _update_rx) = Workspace::testing_stub();
        // The slot is the routing key; the id it is pooled under is the
        // occupant, and the registration's third segment is that id.
        let session_id = "respawn-mode-test";
        let key = SessionSlot::from_str_for_test(session_id);
        let (handle, mut agent_rx) = Workspace::testing_stub_handle();
        workspace.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("Openrouter".to_owned()),
                permission_mode: Some(PermissionMode::BypassPermissions),
                registration: Some(forge_gateway::binding::Registration {
                    org: "Busytools".to_owned(),
                    project: "forge".to_owned(),
                    session: session_id.to_owned(),
                    account: AccountKey("Openrouter".to_owned()),
                    provider: forge_primitives::account::Provider::Openrouter,
                }),
                session_id: session_id.to_owned(),
            },
        );

        let auto_settings = || SessionLaunchSettings {
            settings: Some(serde_json::json!({ "permissions": { "defaultMode": "auto" } })),
            ..SessionLaunchSettings::default()
        };
        workspace
            .dispatch(Command::NewSession {
                key: key.clone(),
                cwd: "/tmp".to_owned(),
                launch_settings: auto_settings(),
            })
            .expect("dispatch new");
        workspace
            .dispatch(Command::ResumeSession {
                key: key.clone(),
                session_id: "old-uuid".to_owned(),
                cwd: "/tmp".to_owned(),
                launch_settings: auto_settings(),
            })
            .expect("dispatch resume");

        let carried_mode = |value: &serde_json::Value| -> Option<String> {
            value
                .get("settings")
                .and_then(|s| s.get(SessionLaunchSettings::PERMISSIONS_KEY))
                .and_then(|p| p.get(SessionLaunchSettings::PERMISSIONS_DEFAULT_MODE_KEY))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let first = agent_rx.try_recv().expect("new session agent command");
        let second = agent_rx.try_recv().expect("resume agent command");
        let forge_primitives::AgentCommand::NewSession {
            session_id: new_id,
            launch_settings: new,
            ..
        } = first
        else {
            panic!("expected a NewSession agent command");
        };
        let forge_primitives::AgentCommand::ResumeSession { launch_settings: resume, .. } = second
        else {
            panic!("expected a ResumeSession agent command");
        };
        assert_eq!(
            carried_mode(&new).as_deref(),
            Some("bypassPermissions"),
            "/new must carry the pooled session's mode, not the TUI session default",
        );
        assert_eq!(
            carried_mode(&resume).as_deref(),
            Some("bypassPermissions"),
            "/resume must carry the pooled session's mode, not the TUI session default",
        );
        // The respawn also re-registers with the gateway: the fresh env
        // set rides the launch settings as overrides, so the respawned
        // child is pointed at this generation's listener - under the id
        // it will run as, not the one it replaces.
        let base_url = |value: &serde_json::Value| -> Option<String> {
            value
                .get("env_overrides")
                .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let minted = new_id.expect("`/new` starts under a minted id");
        assert_eq!(
            base_url(&new).as_deref(),
            Some(
                format!("http://127.0.0.1:{}/Busytools/forge/{minted}", workspace.gateway_port)
                    .as_str()
            ),
            "`/new` stamps the base URL with the id it mints for the child",
        );
        assert_eq!(
            base_url(&resume).as_deref(),
            Some(
                format!("http://127.0.0.1:{}/Busytools/forge/old-uuid", workspace.gateway_port)
                    .as_str()
            ),
            "`/resume` stamps the base URL with the transcript it re-enters",
        );
        assert_eq!(
            workspace.gateway.bindings.binding_for("Busytools", "forge", "old-uuid"),
            Some(AccountKey("Openrouter".to_owned())),
            "the binding follows the occupant the resume leaves running",
        );
        assert_eq!(
            workspace.gateway.bindings.binding_for("Busytools", "forge", &minted),
            None,
            "and the segment the new session left behind is dropped",
        );
        // The overrides carry ONLY the four gateway-owned keys. Carrying
        // the whole env would let a key declared in both layers revert
        // to its account value on respawn.
        let overrides =
            new.get("env_overrides").and_then(|e| e.as_object()).expect("env overrides present");
        assert_eq!(overrides.len(), 4, "exactly the four gateway-owned keys");
    }

    /// `/new` and `/resume` re-spawn on the already-pooled handle, where
    /// the spawn-path stamp never runs - the launch settings must pick
    /// the project's canonical model up at dispatch instead, the way
    /// they pick up the mode.
    #[test]
    fn respawn_commands_on_a_pooled_session_carry_the_project_model() {
        let dir = make_workspace_dir_declaring_model();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let key = SessionSlot::from_str_for_test("respawn-model-test");
        let (handle, mut agent_rx) = Workspace::testing_stub_handle();
        workspace.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("OpenRouter-TM".to_owned()),
                permission_mode: None,
                registration: Some(forge_gateway::binding::Registration {
                    org: "Default".to_owned(),
                    project: "forge".to_owned(),
                    session: key.display(),
                    account: AccountKey("OpenRouter-TM".to_owned()),
                    provider: forge_primitives::account::Provider::Openrouter,
                }),
                session_id: key.display(),
            },
        );

        // The caller's pin is the literal opus - exactly the string the
        // respawn must replace with the project's canonical model.
        let caller_settings = || SessionLaunchSettings {
            settings: Some(serde_json::json!({ "model": "opus" })),
            ..SessionLaunchSettings::default()
        };
        workspace
            .dispatch(Command::NewSession {
                key: key.clone(),
                cwd: "/tmp".to_owned(),
                launch_settings: caller_settings(),
            })
            .expect("dispatch new");
        workspace
            .dispatch(Command::ResumeSession {
                key: key.clone(),
                session_id: "old-uuid".to_owned(),
                cwd: "/tmp".to_owned(),
                launch_settings: caller_settings(),
            })
            .expect("dispatch resume");

        let carried_model = |value: &serde_json::Value| -> Option<String> {
            value
                .get("settings")
                .and_then(|settings| settings.get("model"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let first = agent_rx.try_recv().expect("new session agent command");
        let second = agent_rx.try_recv().expect("resume agent command");
        let forge_primitives::AgentCommand::NewSession { launch_settings: new, .. } = first else {
            panic!("expected a NewSession agent command");
        };
        let forge_primitives::AgentCommand::ResumeSession { launch_settings: resume, .. } = second
        else {
            panic!("expected a ResumeSession agent command");
        };
        assert_eq!(
            carried_model(&new).as_deref(),
            Some("deepseek-v4.1-flash"),
            "/new must carry the project's canonical model, not the caller's pin",
        );
        assert_eq!(
            carried_model(&resume).as_deref(),
            Some("deepseek-v4.1-flash"),
            "/resume must carry the project's canonical model, not the caller's pin",
        );
    }

    /// A session that spawned with no project mode must not gain a
    /// fallback mode at dispatch: the launcher's own session default
    /// survives a respawn.
    #[test]
    fn respawn_commands_on_a_session_with_no_mode_keep_the_launcher_default() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("respawn-modeless-test");
        let (handle, mut agent_rx) = Workspace::testing_stub_handle();
        workspace.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("Plain".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );

        workspace
            .dispatch(Command::NewSession {
                key: key.clone(),
                cwd: "/tmp".to_owned(),
                launch_settings: SessionLaunchSettings {
                    // "plan", not the real launcher default "auto": an Auto
                    // fallback mutant would survive an auto seed.
                    settings: Some(serde_json::json!({ "permissions": { "defaultMode": "plan" } })),
                    ..SessionLaunchSettings::default()
                },
            })
            .expect("dispatch new");

        let forge_primitives::AgentCommand::NewSession { launch_settings, .. } =
            agent_rx.try_recv().expect("new session agent command")
        else {
            panic!("expected a NewSession agent command");
        };
        let carried = launch_settings
            .get("settings")
            .and_then(|s| s.get(SessionLaunchSettings::PERMISSIONS_KEY))
            .and_then(|p| p.get(SessionLaunchSettings::PERMISSIONS_DEFAULT_MODE_KEY))
            .and_then(serde_json::Value::as_str);
        assert_eq!(
            carried,
            Some("plan"),
            "a session with no project mode must keep the launcher's session default",
        );
        // The pool entry carries no registration, so the respawn stays
        // direct: no gateway-owned key rides the overrides, and the
        // child keeps whatever credential its launch settings compose.
        let overrides_empty = launch_settings
            .get("env_overrides")
            .and_then(|e| e.as_object())
            .is_none_or(serde_json::Map::is_empty);
        assert!(
            overrides_empty,
            "a None-registration respawn applies no gateway overrides: {launch_settings}",
        );
    }

    // ---- Session-task routing tests ----
    //
    // A routed command names the slot, and the `SessionTask` is
    // registered under that same slot, so nothing has to be migrated
    // when the occupant changes. What these tests pin is what still
    // moves on its own: pool membership when a task exits, and the
    // binding a slot's occupant reads.

    /// Seed `command_senders`, `pool`, and `domain_handles` at `key`
    /// against a fresh stub handle so `Workspace::dispatch` will
    /// route through the `SessionTask`-style fast path (rather than
    /// the test-only direct fallback). Returns the routed-command
    /// receiver so the test can assert on what flows through.
    fn install_fake_session_task(
        workspace: &Arc<Workspace>,
        key: &SessionSlot,
    ) -> mpsc::UnboundedReceiver<Command> {
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        let arc = Arc::new(handle);
        workspace.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::clone(&arc),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        workspace.command_senders.lock().insert(key.clone(), cmd_tx);
        let domain = workspace.register_domain_session(key.clone(), Some(arc));
        domain.lock().session_id = Some(forge_primitives::SessionId::new(key.display()));
        cmd_rx
    }

    /// A session's `SessionTask` exiting (agent event channel closed -
    /// the subprocess died, e.g. after a cron turn) must RELEASE the
    /// session from the pool + command_senders. Without this the entry
    /// lingers as a dead-but-pooled zombie: the next cron fire's
    /// `running_lead` check still finds it "open" and dispatches a
    /// `Command::Prompt` to the closed channel, which fails with
    /// `SessionClosed` and is silently dropped - so durable crons quietly
    /// stop firing for that project.
    #[tokio::test]
    async fn session_task_exit_releases_dead_pooled_session() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("lead-uuid");
        // Register the session as a connected lead (pool + command_senders
        // + domain_handles).
        let command_rx = install_fake_session_task(&workspace, &key);
        assert!(workspace.pool.lock().contains_key(&key), "precondition: session pooled");

        // In production the SessionTask holds the SAME `Arc<AgentHandle>`
        // that sits in the pool, so the exit-cleanup identity guard
        // (`release_session_if_current`) recognises this task as the
        // current owner. Reuse the pooled handle here rather than a
        // fresh one; its testing-stub event channel is already closed,
        // so `run()` takes the "agent event channel closed" exit path
        // immediately - exactly the post-cron-turn subprocess exit.
        let domain = workspace.domain_session_for(&key).expect("domain registered");
        let pooled_handle = domain.lock().conn.clone().expect("pooled handle on domain");
        let update_tx = UpdateFanout::default();
        let _task_update_rx = update_tx.subscribe(SubscriberRole::Answering);
        let task = crate::session_task::SessionTask {
            key: key.clone(),
            handle: pooled_handle,
            command_rx,
            domain,
            update_tx,
            connected_once: true,
            workspace: Arc::downgrade(&workspace),
            conversation: None,
        };
        task.run().await;

        assert!(
            !workspace.pool.lock().contains_key(&key),
            "pool entry must be released when the SessionTask exits",
        );
        assert!(
            !workspace.command_senders.lock().contains_key(&key),
            "command sender must be released when the SessionTask exits",
        );
    }

    /// A binding is keyed by the segment the child's base URL was
    /// stamped with, which is the id the slot's occupant runs under:
    /// the read follows the live binding rather than the account the
    /// spawn picked.
    #[test]
    fn the_bound_account_follows_the_live_binding_for_the_slot() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("first-uuid");
        install_fake_session_task(&workspace, &key);
        let segment = key.display();
        workspace.pool.lock().get_mut(&key).expect("pooled").registration =
            Some(forge_gateway::binding::Registration {
                org: "Org".to_owned(),
                project: "forge".to_owned(),
                session: segment.clone(),
                account: AccountKey("A".to_owned()),
                provider: forge_primitives::account::Provider::Anthropic,
            });
        workspace.gateway.bindings.bind("Org", "forge", &segment, AccountKey("A".to_owned()));

        assert_eq!(
            workspace.bound_account_for(&key),
            Some(AccountKey("A".to_owned())),
            "the stamped segment is what the binding is keyed by",
        );

        // The gateway re-selecting on the next request is the whole
        // point: the registration's own account is the spawn-time pick
        // and stays "A" here.
        workspace.gateway.bindings.bind("Org", "forge", &segment, AccountKey("B".to_owned()));
        assert_eq!(
            workspace.bound_account_for(&key),
            Some(AccountKey("B".to_owned())),
            "the read follows the live binding, not the spawn-time pick",
        );
    }

    /// Stub carrying one project, `companies`, whose lead session is
    /// already up: catalogued, pooled, and - because
    /// `install_fake_session_task` stamps it - carrying the
    /// `session_id` the retire gate reads. The returned `TempDir`
    /// guards the config dir for the caller's lifetime.
    fn stub_with_connected_lead()
    -> (Arc<Workspace>, mpsc::UnboundedReceiver<SessionUpdate>, SessionSlot, tempfile::TempDir)
    {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("companies");
        fs::create_dir_all(&root).expect("project root");
        let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
        fs::write(
            forge_dir.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "companies"
path = "{root}"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
                root = root.display()
            ),
        )
        .expect("write forge.toml");
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        let (ws, update_rx) = Workspace::testing_stub_with_config(dir.path().to_owned(), config)
            .expect("the stub config's [[slack]] entries are well-formed");

        let lead = SessionSlot::lead("Personal", "companies");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // The store is what a project-rooted spawn resumes from, so the
        // fixture records the lead's id there the way a boot would.
        ws.record_session_id(
            "Personal",
            "companies",
            forge_primitives::LEAD_LABEL,
            &lead.display(),
        );
        ws.record_connected_session(&root.to_string_lossy(), &lead.display(), None);
        let _cmd_rx = install_fake_session_task(&ws, &lead);
        assert_eq!(
            ws.resolve_slot(&SessionTarget::Named("companies".to_owned())).expect("resolves"),
            lead,
            "precondition: the project resolves to the lead this fixture pooled",
        );
        (ws, update_rx, lead, dir)
    }

    /// Every `Spawning` key an update stream carried, in order.
    fn spawning_keys(update_rx: &mut mpsc::UnboundedReceiver<SessionUpdate>) -> Vec<SessionSlot> {
        let mut keys: Vec<SessionSlot> = Vec::new();
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::Spawning { key, .. } = update {
                keys.push(key);
            }
        }
        keys
    }

    /// Any second wake of a live project reaches the fast path: a cron,
    /// a peer prompt, a gotify message, a slack delivery or the boot
    /// auto-start wave landing after one of them. It announces the same
    /// key the live session already holds, so the TUI has one bucket
    /// rather than a second one to retire.
    #[test]
    fn respawn_of_connected_project_announces_the_live_key() {
        let (ws, mut update_rx, lead, _dir) = stub_with_connected_lead();

        crate::spawn::handle_spawn_project(&ws, "companies", SessionLaunchSettings::default());

        let announced = spawning_keys(&mut update_rx);
        assert_eq!(
            announced,
            vec![lead],
            "a re-spawn announces the key the live session already holds, not a second one",
        );
    }

    /// A background rate-limit clears `DomainSession.session_id` while
    /// the pool entry lives. The emit must not read that mirror: the key
    /// it announces is the session's own, whatever the mirror says.
    #[test]
    fn a_rate_limited_session_still_announces_its_own_key() {
        let (ws, mut update_rx, lead, _dir) = stub_with_connected_lead();
        // What `apply_session_update_connection_failed` does to a
        // rate-limited background session: clear the mirror, leave the
        // pool entry alone.
        ws.set_session_id_in_domain(&lead, None);

        crate::spawn::handle_spawn_project(&ws, "companies", SessionLaunchSettings::default());

        assert_eq!(
            spawning_keys(&mut update_rx),
            vec![lead],
            "a cleared session_id must not change the key announced",
        );
    }

    /// `classify_oauth_usage_error` must distinguish HTTP 429 from
    /// auth-related failures so the TUI's bottom-panel hint reads
    /// `rate-limited` (the common case under multiple forge
    /// instances) rather than collapsing every failure to a single
    /// generic bucket.
    #[test]
    fn classify_oauth_usage_error_buckets_known_variants() {
        use forge_gateway::ProbeError;
        use forge_gateway::UsageFetchStatus;
        use forge_primitives::usage::oauth::OauthUsageError;

        let fetch = |err| ProbeError::Fetch(err);
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::HttpStatus(429, String::new()))),
            UsageFetchStatus::RateLimited,
        );
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(60)),
            })),
            UsageFetchStatus::RateLimited,
            "new dedicated 429 variant also maps to RateLimited",
        );
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::RateLimited { retry_after: None })),
            UsageFetchStatus::RateLimited,
        );
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::Unauthorized(401))),
            UsageFetchStatus::Unauthorized,
        );
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::Expired)),
            UsageFetchStatus::Expired,
        );
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::NoCredentials)),
            UsageFetchStatus::Expired,
        );
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::Network("dns".to_owned()))),
            UsageFetchStatus::NetworkFailed,
        );
        // Non-429 HTTP errors and decode failures fall through to the
        // generic `Other` bucket - renderers show "fetch failed" so
        // the user can tell something's wrong without naming a cause.
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::HttpStatus(500, String::new()))),
            UsageFetchStatus::Other,
        );
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::Decode("bad json".to_owned()))),
            UsageFetchStatus::Other,
        );
        // A failed `claude --version` shell-out is a local exec problem,
        // not a reachability verdict, so it must not land in
        // NetworkFailed.
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::UaProbe("no binary".to_owned()))),
            UsageFetchStatus::Other,
        );
        // A scope refusal is anomalous (OAuth tokens carry user:profile)
        // and must not render as an auth failure.
        assert_eq!(
            classify_oauth_usage_error(&fetch(OauthUsageError::ScopeInsufficient)),
            UsageFetchStatus::Other,
        );
        // The backend's own credential miss rides the same Expired
        // bucket as the wire class, and the unmappable 200 - which the
        // callers handle before classifying - keeps the match total.
        assert_eq!(
            classify_oauth_usage_error(&ProbeError::NoCredentials),
            UsageFetchStatus::Expired,
        );
        assert_eq!(
            classify_oauth_usage_error(&ProbeError::Unmappable("no window".to_owned())),
            UsageFetchStatus::Other,
        );
    }

    /// An Anthropic account (setup token on its account block) derives
    /// the token auth class, whose bailed-row repair copy names the
    /// token and never a re-authentication of the shared config dir.
    #[tokio::test]
    async fn a_token_mode_account_derives_the_token_auth_class() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["TokenAcct"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "TokenAcct"
token = "setup-token"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("new");

        let rows = workspace.account_loading_snapshot();
        assert_eq!(rows.len(), 1, "the fixture has one account; got {rows:?}");
        assert_eq!(
            rows[0].auth,
            crate::views::AccountAuth::Token,
            "a setup-token account is the token auth class; got {:?}",
            rows[0].auth,
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // I3 - peer-MCP lifecycle tests
    // ─────────────────────────────────────────────────────────────────

    /// Resolve a project's expanded path from the workspace's view.
    /// Catalog keys derive from `project_key_for_directory(expanded_path)`,
    /// not the literal `~/`-prefixed forge.toml string, so callers that
    /// want `record_connected_session` to populate the right project's
    /// session list need this lookup.
    fn project_expanded_path(workspace: &Workspace, name: &str) -> String {
        workspace.list_projects().into_iter().find(|p| p.name == name).map_or_else(
            || panic!("project '{name}' missing from workspace"),
            |p| p.path.to_string_lossy().into_owned(),
        )
    }

    /// A cron entry carrying what a delivery routes and reads: the id, the
    /// project, the prompt and a description.
    fn test_cron(id: &str, project: &str, prompt: &str) -> forge_primitives::cron::CronEntry {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        CronEntry {
            id: CronId::from(id),
            project_name: project.to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: prompt.to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: Some(format!("{id} summary")),
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        }
    }

    /// A message parked for a seat that never came up is acknowledged to
    /// its sender: the notice lands on the sender's slot and names the
    /// seat that did not take it.
    ///
    /// This is the delivery-ack path, and the parked entry's sender is the
    /// only thing that knows where the notice goes - the ask registry that
    /// used to carry it is gone.
    #[tokio::test]
    async fn notice_undelivered_message_names_the_target_and_reaches_the_sender() {
        use crate::mcp::peers::types::PeerFailureReason;

        let (ws, mut rx) = Workspace::testing_stub();
        ws.enable_test_dispatch_intercept();

        let sender = SessionSlot::from_str_for_test("sender-proj");
        let target = SessionSlot::lead("Default", "gateway-backend");
        ws.notice_undelivered_message(&sender, &target, PeerFailureReason::TargetConnectionFailed);

        let mut echo = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PeerEnvelopeAppended { key, wrapped, .. } = update {
                echo = Some((key, wrapped));
            }
        }
        let (key, notice) = echo.expect("the notice is painted for the sender");
        assert_eq!(key, sender, "the notice lands on the sender, not the target");
        assert_eq!(
            notice.sender_name, "gateway-backend",
            "and names the seat that did not take it"
        );
        assert_eq!(notice.sender_org, "Default", "with that seat's org");
        assert!(
            notice.body.contains("connection lost"),
            "the reason reaches the reader: {}",
            notice.body
        );

        let dispatched = ws.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 1, "the notice is also dispatched as a turn");
        match &dispatched[0] {
            Command::Prompt { key, text, .. } | Command::PromptUnder { key, text, .. } => {
                assert_eq!(*key, sender, "the dispatched turn goes to the sender");
                assert!(text.contains("failed to deliver"), "carrying the failure prose: {text}");
            }
            other => panic!("expected a prompt command, got {other:?}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod workers_state_tests {
    use super::*;
    use crate::mcp::workers::types::WorkerEntry;
    use forge_primitives::WorkerLiveness;
    use std::time::SystemTime;

    fn fake_entry(label: &str, key: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.into(),
            charter: "test charter".into(),
            slot: SessionSlot::from_str_for_test(key),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    #[test]
    fn live_workers_starts_empty() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        assert!(ws.list_live_workers(&project).is_empty());
    }

    #[test]
    fn insert_then_list_returns_entry() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        ws.insert_live_worker(&project, fake_entry("reviewer", "abc"));
        let entries = ws.list_live_workers(&project);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "reviewer");
    }

    #[test]
    fn remove_latest_by_label_picks_most_recent_duplicate() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        ws.insert_live_worker(&project, fake_entry("dup", "old"));
        ws.insert_live_worker(&project, fake_entry("dup", "new"));
        let removed = ws.remove_latest_worker(&project, "dup");
        assert_eq!(removed.unwrap().slot.label(), "new");
        assert_eq!(ws.list_live_workers(&project).len(), 1);
    }

    #[test]
    fn remove_returns_none_when_missing() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        assert!(ws.remove_latest_worker(&project, "missing").is_none());
    }

    /// The atomic guard rejects a second insert for a live label (holding
    /// the lock across the check + push closes the concurrent-dispatch
    /// TOCTOU) and hands back the existing worker.
    #[test]
    fn insert_if_label_absent_rejects_duplicate_label() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        assert!(
            ws.insert_live_worker_if_label_absent(&project, fake_entry("reviewer", "first"), None)
                .is_ok(),
            "the first insert for a label wins",
        );
        let existing = ws
            .insert_live_worker_if_label_absent(&project, fake_entry("reviewer", "second"), None)
            .expect_err("a second live worker for the same label is rejected");
        let existing = match existing {
            LiveWorkerRefusal::LabelLive(session_key) => session_key,
            LiveWorkerRefusal::CleanupPending => panic!("no despawn cleanup is in flight"),
            LiveWorkerRefusal::AtCap { .. } => panic!("no cap was supplied"),
        };
        assert_eq!(existing.label(), "first", "the live holder is returned");
        assert_eq!(ws.list_live_workers(&project).len(), 1, "no duplicate is inserted");
    }

    /// A `Failed` entry does not hold its label - the atomic guard lets
    /// its label be re-spawned (parity with `live_worker_with_label`).
    #[test]
    fn insert_if_label_absent_allows_reinsert_over_failed() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        let mut failed = fake_entry("reviewer", "dead");
        failed.status = WorkerLiveness::Failed;
        ws.insert_live_worker(&project, failed);
        assert!(
            ws.insert_live_worker_if_label_absent(&project, fake_entry("reviewer", "fresh"), None)
                .is_ok(),
            "a Failed entry does not block a re-spawn of its label",
        );
        assert_eq!(ws.list_live_workers(&project).len(), 2);
    }

    #[test]
    fn drain_for_project_clears_and_returns_all() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        ws.insert_live_worker(&project, fake_entry("a", "k1"));
        ws.insert_live_worker(&project, fake_entry("b", "k2"));
        let drained = ws.drain_live_workers(&project);
        assert_eq!(drained.len(), 2);
        assert!(ws.list_live_workers(&project).is_empty());
    }
}

#[cfg(test)]
mod worker_activity_tests {
    use super::*;
    use crate::mcp::workers::types::WorkerEntry;
    use crate::protocol::PendingInteractionSlot;
    use forge_primitives::{SessionLifecycleState as L, WorkerLiveness};
    use std::time::SystemTime;

    fn entry(key: &str, status: WorkerLiveness) -> WorkerEntry {
        WorkerEntry {
            label: "implementer".into(),
            charter: "test charter".into(),
            slot: SessionSlot::from_str_for_test(key),
            session_id: None,
            status,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// The same derivation has to be reachable by slot: a project lead has
    /// no `WorkerEntry` to hand `worker_activity`, and a view drawing the
    /// lead's row needs its state from somewhere.
    #[test]
    fn session_activity_reads_a_slot_with_no_worker_entry() {
        let (ws, _rx) = Workspace::testing_stub();

        assert_eq!(
            ws.session_activity(&SessionSlot::from_str_for_test("s-gone")),
            L::Sleeping,
            "a slot with no domain session is asleep, not idle",
        );

        let blocked = SessionSlot::from_str_for_test("s-blocked");
        let domain = ws.register_domain_session(blocked.clone(), None);
        {
            let mut guard = domain.lock();
            guard.turn_pending = true;
            let (tx, _rx) = tokio::sync::oneshot::channel();
            guard.pending_interactions.insert("tool-1".to_owned(), testing::test_permission(tx));
        }
        assert_eq!(
            ws.session_activity(&blocked),
            L::Attention,
            "a turn in flight holding a pending interaction needs a person",
        );
    }

    /// Two states the lifecycle derivation has to reach or a view cannot
    /// draw them: a session held on `/login`, and one whose last attempt
    /// to start failed and has not been retried. Both are facts about the
    /// session, so the core answers them rather than each view folding
    /// them out of the events it happened to see.
    #[test]
    fn session_activity_reaches_login_and_failure() {
        let (ws, _rx) = Workspace::testing_stub();
        let held = SessionSlot::from_str_for_test("s-login");
        let domain = ws.register_domain_session(held.clone(), None);
        assert_eq!(ws.session_activity(&held), L::Idle, "a fresh domain is idle");

        {
            let mut guard = domain.lock();
            guard.awaiting_login = true;
            // A login wait is not a turn, so a state that claims one is in
            // flight must not outrank it.
            guard.turn_pending = true;
        }
        assert_eq!(
            ws.session_activity(&held),
            L::AuthRequired,
            "a session waiting on /login cannot proceed, whatever its turn state says",
        );

        // The signal that actually arrives: a turn that died on a missing
        // credential. `AgentEvent::AuthRequired` is constructed nowhere in
        // the tree, so without this the state is unreachable and the row
        // draws a calm idle for a session held on `/login`.
        let by_error = SessionSlot::from_str_for_test("s-login-error");
        let domain = ws.register_domain_session(by_error.clone(), None);
        let auth_message = "authentication failed: please log in";
        crate::session_task::apply_event_to_domain(
            &mut domain.lock(),
            &forge_agent::client::AgentEvent::TurnError {
                session_id: "uuid".to_owned(),
                message: auth_message.to_owned(),
                class: forge_agent::translate::error_handling::classify_turn_error(auth_message),
            },
        );
        assert_eq!(
            ws.session_activity(&by_error),
            L::AuthRequired,
            "a turn error the classifier calls auth-required holds the session on /login",
        );

        // A failed attempt is recorded where the failure releases the
        // session, so it survives the release.
        let dead = SessionSlot::from_str_for_test("s-dead");
        ws.record_spawn_failure(&dead, "OAuth token expired");
        assert_eq!(ws.session_activity(&dead), L::Failed, "an unrecovered failure is failed");
        assert_eq!(
            ws.spawn_failure(&dead).as_deref(),
            Some("OAuth token expired"),
            "and carries why",
        );

        // Registering a domain is not what clears it. Production hoists a
        // domain itself rather than registering one, so the clear lives in
        // the `Connected` arm - see `a_connection_failure_is_recorded_and_
        // a_connected_clears_it`, which drives a real task through both.
        ws.register_domain_session(dead.clone(), None);
        assert_eq!(
            ws.spawn_failure(&dead).as_deref(),
            Some("OAuth token expired"),
            "registering a domain does not clear the record, because production never registers one",
        );
        assert_eq!(
            ws.session_activity(&dead),
            L::Failed,
            "so the slot still reads failed until something connects",
        );
    }

    /// What a session is waiting on a person for, which is what a
    /// needs-you row has to name. A question outranks a permission prompt,
    /// so a slot holding both reads as the question.
    #[test]
    fn pending_interaction_answers_the_kind_a_slot_is_waiting_on() {
        let (ws, _rx) = Workspace::testing_stub();
        let held = |name: &str, slots: Vec<PendingInteractionSlot>| {
            let key = SessionSlot::from_str_for_test(name);
            let domain = ws.register_domain_session(key.clone(), None);
            {
                let mut guard = domain.lock();
                for (index, slot) in slots.into_iter().enumerate() {
                    guard.pending_interactions.insert(format!("{name}-{index}"), slot);
                }
            }
            key
        };
        let permission = || {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            testing::test_permission(tx)
        };
        let question = || {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            testing::test_question(tx)
        };

        assert_eq!(
            ws.pending_interaction(&SessionSlot::from_str_for_test("p-none")),
            None,
            "a slot with no domain session is waiting on nothing",
        );

        let asked = held("p-question", vec![question()]);
        assert_eq!(
            ws.pending_interaction(&asked),
            Some(PendingInteractionKind::Question),
            "a held question is what the slot is waiting on",
        );

        let prompted = held("p-permission", vec![permission()]);
        assert_eq!(
            ws.pending_interaction(&prompted),
            Some(PendingInteractionKind::Permission),
            "a permission prompt alone is what the slot is waiting on",
        );

        let both = held("p-both", vec![permission(), question()]);
        assert_eq!(
            ws.pending_interaction(&both),
            Some(PendingInteractionKind::Question),
            "a question outranks the permission prompt beside it",
        );
    }

    /// **A parked browser hand-off is in the READ as well as on the stream**:
    /// a view that attached after it landed has nothing else to draw the dock
    /// from, and the dock is the only place its answer can come from.
    #[test]
    fn a_parked_hand_off_is_in_the_pending_asks_read() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("caller-uuid");
        let handoff = forge_primitives::browser::HandOff {
            id: uuid::Uuid::new_v4(),
            reason: "solve the CAPTCHA".to_owned(),
            profile: None,
        };
        let id = handoff.id;
        let (_asked, _answer) = ws.register_browser_hand_off(&key, handoff);

        assert!(
            ws.pending_asks(&key)
                .iter()
                .any(|ask| matches!(ask, crate::protocol::PendingAsk::BrowserHandOff(held)
                    if held.id == id)),
            "the hand-off is in the read a late view makes",
        );
        let other = SessionSlot::from_str_for_test("other-uuid");
        assert!(
            !ws.pending_asks(&other)
                .iter()
                .any(|ask| matches!(ask, crate::protocol::PendingAsk::BrowserHandOff(_))),
            "and another seat's read does not carry it",
        );
    }

    /// What a view that attached after the prompt landed reads: the requests
    /// the core kept beside the answer's oneshots, which is the only place
    /// they survive a stream that carries each once.
    ///
    /// **The list in arrival order**, because a parallel batch parks several
    /// at once and the read is all a mid-batch attach has - and the dock
    /// draws the front, which is the oldest, so the order is the read's own
    /// contract and not a reader's choice.
    #[test]
    fn pending_asks_reads_the_requests_the_core_kept_in_order() {
        let (ws, _rx) = Workspace::testing_stub();
        let held = |name: &str, slots: Vec<PendingInteractionSlot>| {
            let key = SessionSlot::from_str_for_test(name);
            let domain = ws.register_domain_session(key.clone(), None);
            {
                let mut guard = domain.lock();
                for (index, slot) in slots.into_iter().enumerate() {
                    guard.pending_interactions.insert(format!("{name}-{index}"), slot);
                }
            }
            key
        };
        let permission = || {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            testing::test_permission(tx)
        };
        let question = || {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            testing::test_question(tx)
        };

        assert!(
            ws.pending_asks(&SessionSlot::from_str_for_test("a-none")).is_empty(),
            "a slot holding nothing kept nothing to read back",
        );

        let prompted = held("a-permission", vec![permission()]);
        let asks = ws.pending_asks(&prompted);
        assert_eq!(asks.len(), 1, "a held permission prompt reads back");
        assert!(
            matches!(asks[0], crate::protocol::PendingAsk::Permission(_)),
            "and reads back as the kind it is",
        );
        assert_eq!(
            asks[0].tool_id(),
            Some(testing::TEST_TOOL_ID),
            "naming the call an answer addresses"
        );

        let asked = held("a-question", vec![question()]);
        assert!(
            matches!(ws.pending_asks(&asked)[0], crate::protocol::PendingAsk::Question(_)),
            "a held question reads back as the kind it is",
        );

        // A parallel batch, in the order the requests parked: the read hands
        // both back with the permission prompt leading, because it parked
        // first - the terminal's own queue rule.
        let both = held("a-both", vec![permission(), question()]);
        let asks = ws.pending_asks(&both);
        assert_eq!(asks.len(), 2, "both asks of a parallel batch read back");
        assert!(
            matches!(asks[0], crate::protocol::PendingAsk::Permission(_))
                && matches!(asks[1], crate::protocol::PendingAsk::Question(_)),
            "oldest first, which is the front a dock draws",
        );

        // A repeated call id replaces in place rather than parking twice: the
        // set is keyed by the call an answer addresses.
        let repeated = SessionSlot::from_str_for_test("a-repeat");
        let domain = ws.register_domain_session(repeated.clone(), None);
        {
            let mut guard = domain.lock();
            guard.pending_interactions.insert("a-call".to_owned(), permission());
            guard.pending_interactions.insert("a-call".to_owned(), question());
        }
        let asks = ws.pending_asks(&repeated);
        assert_eq!(asks.len(), 1, "a repeated call id does not park twice");
        assert!(
            matches!(asks[0], crate::protocol::PendingAsk::Question(_)),
            "and the newer slot is the one held",
        );
    }

    /// The statuspage answer is the same for every viewer, and it has to
    /// outlive the view that used to fetch it: the terminal owned this
    /// probe, so without moving it a client built after the terminal is
    /// gone would never see a service status at all. The answer is held as
    /// well as announced, so a view that attached after the one update that
    /// carried it can still read it.
    #[tokio::test]
    async fn the_service_status_is_held_and_announced() {
        use forge_primitives::cloud::service_status::{ServiceIssue, ServiceSeverity};

        let (ws, mut rx) = Workspace::testing_stub();
        let found = ServiceIssue {
            severity: ServiceSeverity::Warning,
            message: "Claude Code status: degraded.".to_owned(),
        };
        let prober: ServiceStatusProber = {
            let found = found.clone();
            Box::new(move || {
                let found = found.clone();
                Box::pin(async move { Some(found) })
            })
        };

        assert!(ws.service_status().is_none(), "nothing has been probed yet");

        run_service_status_probe(
            std::sync::Arc::clone(&ws.service_status),
            ws.update_tx().clone(),
            prober,
        )
        .await;

        assert_eq!(
            ws.service_status(),
            Some(found),
            "the answer the probe found is the answer the read hands back",
        );
        assert!(
            matches!(rx.try_recv(), Ok(SessionUpdate::ServiceStatus { .. })),
            "and the one update that carried it still goes out",
        );
    }

    /// A seat can be held on a Slack draft, which the registry parks on a
    /// reply exactly the way a permission or a question is parked - the
    /// same shape, the same awaited answer, a different kind.
    ///
    /// The record had no room for it, so a view that attached after the
    /// draft landed could not read that one was waiting: a record named
    /// for a category has to carry every member of it, or the name says
    /// it is complete when it is not. A draft leads the list, which is the
    /// precedence the read has always given it - it is held in its own
    /// registry rather than in the session's pending set.
    #[test]
    fn a_parked_slack_draft_reads_back_as_the_third_kind() {
        let (ws, _rx) = Workspace::testing_stub();
        let seat = SessionSlot::from_str_for_test("a-draft");
        let draft = forge_primitives::slack::SlackDraft {
            id: uuid::Uuid::new_v4(),
            workspace: "acme".to_owned(),
            conversation: "C1".to_owned(),
            conversation_label: "acme".to_owned(),
            thread_ts: None,
            text: "hello".to_owned(),
            tool: "slack__post".to_owned(),
        };
        let (_id, _decision) = ws.register_slack_draft(&seat, draft);

        let asks = ws.pending_asks(&seat);
        assert_eq!(asks.len(), 1, "a parked draft reads back");
        assert!(
            matches!(asks[0], crate::protocol::PendingAsk::SlackDraft(_)),
            "and reads back as the kind it is",
        );
        assert!(
            asks[0].tool_id().is_none(),
            "a draft is answered by its own id, so it names no tool call",
        );
        assert!(
            ws.pending_asks(&SessionSlot::from_str_for_test("a-bystander")).is_empty(),
            "and a seat holding nothing is not handed another seat's draft",
        );

        // The draft leads a question parked beside it, which is what the
        // single read did before the list existed.
        let domain = ws.register_domain_session(seat.clone(), None);
        let (tx, _rx) = tokio::sync::oneshot::channel();
        domain.lock().pending_interactions.insert("a-call".to_owned(), testing::test_question(tx));
        assert!(
            matches!(ws.pending_asks(&seat)[0], crate::protocol::PendingAsk::SlackDraft(_)),
            "a draft answers first, ahead of the question waiting behind it",
        );
    }

    /// Background work is a fact about the session rather than about who
    /// is looking, so the read is slot-shaped and answers from the slot's
    /// own domain: a lead has background work too.
    #[test]
    fn background_work_is_read_from_the_slots_own_domain() {
        let (ws, _rx) = Workspace::testing_stub();
        let absent = SessionSlot::from_str_for_test("bg-absent");
        assert!(
            !ws.has_background_work(&absent),
            "a slot with no domain session has no live background work",
        );

        let live = SessionSlot::from_str_for_test("bg-live");
        let domain = ws.register_domain_session(live.clone(), None);
        assert!(!ws.has_background_work(&live), "a fresh domain has none");

        domain.lock().background_work = true;
        assert!(ws.has_background_work(&live), "the read answers the slot's own registry");
        assert!(
            !ws.has_background_work(&absent),
            "one slot's background work must not answer for another",
        );
    }

    /// The whole point of the field: a worker that finished its turn is
    /// still `WorkerLiveness::Running`, so a lead polling `agents__list`
    /// used to have no way to tell it from one mid-turn.
    #[test]
    fn connected_worker_with_no_turn_in_flight_reports_idle() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-idle");
        ws.register_domain_session(key.clone(), None);
        let worker = entry("w-idle", WorkerLiveness::Running);

        assert_eq!(
            ws.worker_activity(&worker),
            L::Idle,
            "liveness Running with no turn in flight is idle, not working",
        );
    }

    /// **A turn that opens with the model's own output reads in flight.**
    ///
    /// The wire header went idle while a worker was demonstrably working:
    /// `turn_pending` is spent by the previous `Result` and
    /// `session_state_changed` no longer arrives, so thinking and prose left
    /// it false for 41 seconds (measured on the seat, 2026-10-09) while the
    /// terminal showed the turn running. The model's own frames are the proof
    /// the terminal reads; this is that reading on the wire's side.
    #[test]
    fn a_turn_opening_with_the_models_own_output_reads_in_flight() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-opening");
        let domain = ws.register_domain_session(key, None);
        let mut guard = domain.lock();

        // The previous turn's Result has landed, which is the state the header
        // was stuck in: the routed-prompt stamp is spent and nothing else has
        // said a turn is open.
        guard.turn_pending = false;
        assert!(!guard.turn_in_flight(), "precondition: nothing yet says a turn is open");

        guard.note_liveness(crate::domain_session::TurnLiveness::Working);
        assert!(guard.turn_in_flight(), "the model is producing output, so a turn is open");
    }

    /// **The mapping itself, not just the setter.**
    ///
    /// The two tests above drive `note_liveness` directly, so they pin the
    /// predicate and stop short of the fold that feeds it: deleting the
    /// `ThinkingTokens` arm here leaves them both green while a turn that
    /// opens with thinking regresses to the 41-second idle window the mirror
    /// exists to close. This is the arm, so it is the one pinned.
    #[test]
    fn a_frame_that_proves_a_turn_maps_to_working() {
        let counter = forge_primitives::Message::ThinkingTokens {
            estimated_tokens: 40,
            estimated_tokens_delta: 40,
            uuid: "tokens-1".to_owned(),
            session_id: "s".to_owned(),
            extras: serde_json::Map::new(),
        };
        assert_eq!(
            crate::domain_session::liveness_of(&counter),
            Some(crate::domain_session::TurnLiveness::Working),
            "the model's own output opens a turn",
        );

        // A report ABOUT the turn is not a liveness signal, and
        // `session_state_changed` is the subtype to pin: it is the one the fold
        // already parses, so an arm reading it as "running" would otherwise
        // survive every test there is.
        let report = forge_primitives::Message::System {
            subtype: "session_state_changed".to_owned(),
            session_id: None,
            data: serde_json::Value::Null,
        };
        assert_eq!(
            crate::domain_session::liveness_of(&report),
            None,
            "a system report is not a liveness signal",
        );
    }

    /// The same mirror closes: a `Result` ends the turn the frames opened.
    #[test]
    fn a_result_closes_the_turn_the_frames_opened() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-closing");
        let domain = ws.register_domain_session(key, None);
        let mut guard = domain.lock();

        guard.note_liveness(crate::domain_session::TurnLiveness::Working);
        assert!(guard.turn_in_flight(), "precondition: the frames opened a turn");

        guard.note_liveness(crate::domain_session::TurnLiveness::Ended);
        assert!(!guard.turn_in_flight(), "the Result ended it");
    }

    /// Every state the derivation can land on, including the precedence
    /// that matters: a pending interaction outranks the in-flight turn it
    /// is blocking, otherwise the deadlock stays invisible.
    #[test]
    fn worker_activity_covers_every_derived_state() {
        let (ws, _rx) = Workspace::testing_stub();

        let with_domain = |name: &str, f: &dyn Fn(&mut DomainSession)| {
            let key = SessionSlot::from_str_for_test(name);
            let domain = ws.register_domain_session(key, None);
            f(&mut domain.lock());
            ws.worker_activity(&entry(name, WorkerLiveness::Running))
        };

        assert_eq!(
            with_domain("w-pending", &|d| d.turn_pending = true),
            L::Running,
            "turn_pending is the synchronous turn-start marker",
        );
        assert_eq!(
            with_domain("w-wire-running", &|d| {
                d.runtime_state = Some(forge_primitives::RuntimeSessionState::Running);
            }),
            L::Running,
            "the wire-confirmed Running state counts even before turn_pending",
        );
        // Unreachable today rather than merely untested: nothing in the
        // tree emits `requires_action`, and `session_state_changed`
        // itself is in no wire-conformance baseline. Pinned so the
        // mapping is already right if the CLI ever sends it.
        assert_eq!(
            with_domain("w-wire-action", &|d| {
                d.runtime_state = Some(forge_primitives::RuntimeSessionState::RequiresAction);
            }),
            L::Attention,
            "RequiresAction is the CLI asking for a human, not a turn making progress",
        );
        assert_eq!(
            with_domain("w-blocked", &|d| {
                d.turn_pending = true;
                let (tx, _rx) = tokio::sync::oneshot::channel();
                d.pending_interactions.insert("tool-1".to_owned(), testing::test_permission(tx));
            }),
            L::Attention,
            "a pending interaction outranks the turn it is blocking",
        );

        // No DomainSession at all: the subprocess is gone even though the
        // registry still lists the worker.
        assert_eq!(ws.worker_activity(&entry("w-gone", WorkerLiveness::Running)), L::Sleeping);

        // Liveness that already answers the question passes straight
        // through - neither has a connected session to interrogate.
        assert_eq!(ws.worker_activity(&entry("w-spawning", WorkerLiveness::Spawning)), L::Spawning);
        assert_eq!(ws.worker_activity(&entry("w-failed", WorkerLiveness::Failed)), L::Failed);
    }

    /// Contract, not a reproduction: `Attention` is only ever truthful
    /// while a turn is in flight, so a held slot without one reads
    /// `Idle`. No production route is known to reach this state - it
    /// pins the invariant, so a slot that outlives its turn can never
    /// pin a worker at `Attention` for the life of the session.
    #[test]
    fn stranded_interaction_slot_on_an_idle_session_reports_idle() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-stranded");
        let domain = ws.register_domain_session(key, None);
        {
            let mut guard = domain.lock();
            // A held slot with no turn: the shape the invariant forbids.
            guard.turn_pending = false;
            let (tx, _rx) = tokio::sync::oneshot::channel();
            guard
                .pending_interactions
                .insert("tool-stranded".to_owned(), testing::test_permission(tx));
        }

        assert_eq!(
            ws.worker_activity(&entry("w-stranded", WorkerLiveness::Running)),
            L::Idle,
            "a held slot with no turn in flight is incoherent state, not a worker awaiting input",
        );
    }

    /// A parked Slack draft is a pending interaction like any other: the
    /// seat is waiting on a person, so the lifecycle has to say
    /// `Attention` - or every view that draws its needs mark from the
    /// lifecycle (the client's) stays quiet while the dock sits
    /// unanswered. The draft lives in the workspace's own registry rather
    /// than the session's pending set, which is exactly why this arm needs
    /// the read of its own.
    #[tokio::test]
    async fn a_parked_slack_draft_reads_as_attention() {
        let (ws, _rx) = Workspace::testing_stub();
        let seat = SessionSlot::from_str_for_test("w-draft");
        let domain = ws.register_domain_session(seat.clone(), None);
        domain.lock().turn_pending = true;
        assert_eq!(
            ws.session_activity(&seat),
            L::Running,
            "a turn advancing on its own is Running, which is the control",
        );

        let draft = forge_primitives::slack::SlackDraft {
            id: uuid::Uuid::new_v4(),
            workspace: "acme".to_owned(),
            conversation: "C1".to_owned(),
            conversation_label: "acme".to_owned(),
            thread_ts: None,
            text: "hello".to_owned(),
            tool: "slack__post".to_owned(),
        };
        let (_id, _decision) = ws.register_slack_draft(&seat, draft);

        assert_eq!(
            ws.session_activity(&seat),
            L::Attention,
            "a held draft is a person's to answer, not a running turn's",
        );

        // A draft belongs to the seat that asked: another seat holding
        // nothing of its own stays Running.
        let bystander = SessionSlot::from_str_for_test("w-bystander");
        let other = ws.register_domain_session(bystander.clone(), None);
        other.lock().turn_pending = true;
        assert_eq!(
            ws.session_activity(&bystander),
            L::Running,
            "another seat's draft is not this seat's news",
        );
    }

    /// A parked browser hand-off is a pending interaction like any other:
    /// the seat waits on a person, so the lifecycle says `Attention` - every
    /// needs mark draws from this. Ved, live round 2026-10-07: the hand-off
    /// dock waited while the rail and the tabs stayed quiet. Same shape as
    /// the parked draft's arm, registry read and all.
    #[tokio::test]
    async fn a_parked_browser_hand_off_reads_as_attention() {
        let (ws, _rx) = Workspace::testing_stub();
        let seat = SessionSlot::from_str_for_test("w-handoff");
        let domain = ws.register_domain_session(seat.clone(), None);
        domain.lock().turn_pending = true;
        assert_eq!(ws.session_activity(&seat), L::Running, "the control, as for the draft");

        let handoff = forge_primitives::browser::HandOff {
            id: uuid::Uuid::new_v4(),
            reason: "solve the CAPTCHA".to_owned(),
            profile: None,
        };
        let (_id, _answer) = ws.register_browser_hand_off(&seat, handoff);

        assert_eq!(
            ws.session_activity(&seat),
            L::Attention,
            "a held hand-off is a person's to answer, not a running turn's",
        );

        let bystander = SessionSlot::from_str_for_test("w-bystander2");
        let other = ws.register_domain_session(bystander.clone(), None);
        other.lock().turn_pending = true;
        assert_eq!(
            ws.session_activity(&bystander),
            L::Running,
            "another seat's hand-off is not this seat's news",
        );
    }

    /// `activity` is populated by the `agents__list` read path only; the
    /// `WorkerStatusChanged` event path leaves it `None`.
    #[test]
    fn event_path_leaves_activity_none() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = SessionSlot::from_str_for_test("w-both");
        ws.register_domain_session(key.clone(), None);
        let worker = entry("w-both", WorkerLiveness::Running);

        assert_eq!(worker.to_status().activity, None, "the event path derives no activity");
        assert_eq!(
            ws.worker_status_snapshot(&worker).activity,
            Some(L::Idle),
            "the read path always derives one",
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod release_session_cascade_tests {
    use super::*;
    use crate::mcp::workers::types::WorkerEntry;
    use forge_primitives::WorkerLiveness;
    use std::fs;
    use std::time::SystemTime;
    use tempfile::tempdir;

    /// The `forge` project's lead slot, the slot the fixtures in this
    /// module release to trigger the cascade.
    fn lead_slot() -> SessionSlot {
        SessionSlot::lead("Default", "forge")
    }

    fn fake_entry(label: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.into(),
            charter: "test charter".into(),
            slot: SessionSlot::worker("Default", "forge", label),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: lead_slot(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    fn make_workspace_dir() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// `release_session_with_cascade` cascades worker termination when
    /// the released session is a project's lead: its slot carries the
    /// lead label and names a declared project. Workers' JSONLs persist
    /// on disk (we don't delete them); only the in-memory live_workers
    /// entries + the running session subprocesses are torn down.
    #[tokio::test]
    async fn release_session_on_lead_cascades_workers() {
        let dir = make_workspace_dir();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut rx = workspace.subscribe();

        let project = workspace.list_projects().into_iter().next().expect("forge project");
        let project_key = project.key.clone();
        let lead_key = lead_slot();

        // Insert two workers under the project.
        workspace.insert_live_worker(&project_key, fake_entry("r1"));
        workspace.insert_live_worker(&project_key, fake_entry("r2"));
        assert_eq!(workspace.list_live_workers(&project_key).len(), 2);

        // Release the lead. Cascade fires before the lead release
        // itself; workers' live_workers entries are gone afterward.
        workspace.release_session_with_cascade(&lead_key);
        assert!(
            workspace.list_live_workers(&project_key).is_empty(),
            "workers must be cascade-closed"
        );

        // Drain the channel and confirm we saw a Removed
        // WorkerStatusChanged for each worker.
        let mut removed_count = 0;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::WorkerStatusChanged { action, .. } = update
                && action == crate::protocol::WorkerStatusAction::Removed
            {
                removed_count += 1;
            }
        }
        assert_eq!(removed_count, 2, "two Removed events fire for the two workers");
    }

    /// **The release is announced before anything is torn down** (#1930).
    /// A viewer marks the seat from this frame, so it has to be out ahead of
    /// the cascade: announced after, a lead's row would keep reading as
    /// working for exactly the seconds the teardown takes - and a close made
    /// from another view has no other frame at all.
    #[tokio::test]
    async fn a_release_is_announced_before_the_cascade_runs() {
        let dir = make_workspace_dir();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut rx = workspace.subscribe();

        let project = workspace.list_projects().into_iter().next().expect("forge project");
        let project_key = project.key.clone();
        let lead_key = lead_slot();
        workspace.insert_live_worker(&project_key, fake_entry("r1"));

        workspace.release_session_with_cascade(&lead_key);

        let announced: Vec<SessionUpdate> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let first = announced.first().expect("the release announces something");
        assert!(
            matches!(first, SessionUpdate::Releasing { key } if key == &lead_key),
            "the lead's release is the first thing announced: {first:?}",
        );
        assert!(
            announced.iter().any(|update| matches!(
                update,
                SessionUpdate::WorkerStatusChanged { action, .. }
                    if *action == crate::protocol::WorkerStatusAction::Removed
            )),
            "and the cascade's own removal follows it: {announced:?}",
        );
    }

    /// `release_session_with_cascade` on a non-lead (or unknown) session
    /// is a plain release with NO cascade. Confirms the cascade is gated on
    /// "session is a lead of some project."
    #[tokio::test]
    async fn release_session_on_non_lead_does_not_cascade() {
        let dir = make_workspace_dir();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));

        let project_key = workspace.list_projects().into_iter().next().expect("forge").key;
        workspace.insert_live_worker(&project_key, fake_entry("r1"));

        // A worker of the very project the lead slot above names: it
        // resolves, so only the label keeps the cascade off.
        let unknown = SessionSlot::worker("Default", "forge", "r1");
        workspace.release_session_with_cascade(&unknown);
        assert_eq!(
            workspace.list_live_workers(&project_key).len(),
            1,
            "non-lead release must not cascade"
        );
    }

    /// The cascade is gated on the slot's own label, not on where the
    /// session sits in the catalog. A worker whose Connected landed it
    /// at `catalog[0]` is still a worker, so closing it must not drain
    /// its peers; the lead's slot, released with the same catalog order,
    /// still cascades.
    #[tokio::test]
    async fn release_session_cascades_when_worker_sits_at_catalog_head() {
        let dir = make_workspace_dir();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));

        let project = workspace.list_projects().into_iter().next().expect("forge project");
        let project_key = project.key.clone();
        let project_path = project.path.to_string_lossy().into_owned();

        // Seed the lead FIRST, then the worker. record_connected_session
        // inserts at index 0, so after both calls catalog[0] is the
        // worker and catalog[1] is the lead.
        let lead_key = lead_slot();
        let worker_key = SessionSlot::worker("Default", "forge", "reviewer");
        workspace.record_connected_session(&project_path, &lead_key.display(), None);
        workspace.record_connected_session(&project_path, &worker_key.display(), None);
        workspace.insert_live_worker(&project_key, fake_entry("reviewer"));

        // Sanity: catalog head is the worker, not the lead.
        let projects_now = workspace.list_projects();
        let first_session = &projects_now[0].sessions[0].session;
        assert_eq!(
            first_session.as_str(),
            worker_key.display(),
            "catalog[0] must be the worker for the discrimination to bite"
        );

        // Closing the WORKER: its slot carries the worker's label, so
        // nothing reads it as its lead however the registry is ordered.
        workspace.release_session_with_cascade(&worker_key);
        assert_eq!(
            workspace.list_live_workers(&project_key).len(),
            1,
            "a worker's slot is never mistaken for its lead, whatever the catalog order"
        );

        // Release the LEAD, with the catalog still ordering the worker
        // first: the label is what cascades.
        workspace.release_session_with_cascade(&lead_key);

        assert!(
            workspace.list_live_workers(&project_key).is_empty(),
            "lead release must cascade even when a worker sits at catalog[0]"
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tag_retry_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build the on-disk `<config_dir>/projects/<sanitized_cwd>/`
    /// directory `tag_session` expects and return the path the JSONL
    /// would land at. Caller decides when to actually create the file.
    fn jsonl_path_for(
        config_dir: &std::path::Path,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> std::path::PathBuf {
        let sanitized = forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            &cwd.to_string_lossy(),
        ));
        let project_dir = forge_sdk::projects_dir_for(config_dir).join(&sanitized);
        fs::create_dir_all(&project_dir).expect("project dir");
        project_dir.join(format!("{session_id}.jsonl"))
    }

    /// JSONL exists from the start: tag_session_with_retry succeeds on
    /// the first attempt and appends a tag row.
    #[tokio::test]
    async fn succeeds_immediately_when_jsonl_exists() {
        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let session_id = "550e8400-e29b-41d4-a716-446655440001";
        let path = jsonl_path_for(cfg.path(), cwd.path(), session_id);
        fs::write(&path, "").expect("seed jsonl");

        let result = tag_session_with_retry(
            cfg.path(),
            session_id,
            "forge:worker:smoke",
            &cwd.path().to_string_lossy(),
            5,
            Duration::from_millis(10),
        )
        .await;
        assert!(result.is_ok(), "tag should succeed: {result:?}");
        let body = fs::read_to_string(&path).expect("read");
        assert!(body.contains("\"tag\":\"forge:worker:smoke\""), "tag row appended: {body:?}");
    }

    /// JSONL appears after a few attempts: retry loop wins. Spawns the
    /// retry, sleeps long enough for it to hit `NotFound` once or twice,
    /// then creates the file and observes a successful tag.
    #[tokio::test]
    async fn retries_until_jsonl_appears() {
        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let session_id = "550e8400-e29b-41d4-a716-446655440002";
        let path = jsonl_path_for(cfg.path(), cwd.path(), session_id);
        // File doesn't exist yet; create it after a brief delay.
        let path_clone = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            fs::write(&path_clone, "").expect("create jsonl");
        });

        let result = tag_session_with_retry(
            cfg.path(),
            session_id,
            "forge:worker:smoke",
            &cwd.path().to_string_lossy(),
            30,
            Duration::from_millis(50),
        )
        .await;
        assert!(result.is_ok(), "retry should win: {result:?}");
        let body = fs::read_to_string(&path).expect("read");
        assert!(body.contains("\"tag\":\"forge:worker:smoke\""), "tag row appended: {body:?}");
    }

    /// JSONL never appears: retry exhausts and surfaces NotFound.
    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let session_id = "550e8400-e29b-41d4-a716-446655440003";
        let _path = jsonl_path_for(cfg.path(), cwd.path(), session_id);
        // Don't create the JSONL.

        let result = tag_session_with_retry(
            cfg.path(),
            session_id,
            "forge:worker:smoke",
            &cwd.path().to_string_lossy(),
            3,
            Duration::from_millis(10),
        )
        .await;
        match result {
            Err(forge_sdk::Error::Io(io_err)) => {
                assert_eq!(io_err.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected Io(NotFound), got {other:?}"),
        }
    }

    /// Invalid UUID is a non-NotFound error: must propagate immediately
    /// without retrying.
    #[tokio::test]
    async fn invalid_uuid_propagates_without_retry() {
        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let start = std::time::Instant::now();
        let result = tag_session_with_retry(
            cfg.path(),
            "not-a-valid-uuid",
            "forge:worker:smoke",
            &cwd.path().to_string_lossy(),
            30,
            Duration::from_millis(500),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(forge_sdk::Error::MessageParse { .. })));
        // Should NOT have slept through 30 * 500ms = 15s of retries.
        assert!(
            elapsed < Duration::from_secs(1),
            "non-NotFound errors must skip retry: {elapsed:?}"
        );
    }

    /// A `Spawning` worker entry keyed by the slot its spawn names -
    /// the triple the tag path looks the entry up by. The slot does not
    /// move across `/new`, so a re-keyed worker keeps the one it was
    /// spawned under.
    fn fake_spawning_entry(
        label: &str,
        session_key: &SessionSlot,
        needs_tag: bool,
    ) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.into(),
            charter: "test".into(),
            slot: session_key.clone(),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Spawning,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", "forge"),
            needs_tag,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// `apply_worker_tag_or_rollback_with_config_dir`: when the JSONL
    /// never appears, the retry exhausts with NotFound, but the worker
    /// stays in `live_workers` (NO rollback), transitions to Running,
    /// and `needs_tag` remains true so the opportunistic retry on the
    /// first turn can try again. A single `StatusChanged` event fires.
    ///
    /// The durable row stays too, and it is the store that makes that
    /// assertion mean something: the worker is live and Running, so a
    /// delete reaching this arm would drop the only handle on a worker
    /// nothing has rolled back.
    #[tokio::test]
    async fn notfound_keeps_worker_with_needs_tag_flag() {
        let (workspace, mut rx) = Workspace::testing_stub();
        workspace.seed_test_project("forge", "/tmp/notfound-tag-keeps");
        let db_dir = tempdir().expect("db tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                "/tmp/notfound-tag-keeps",
            )));
        let session_id = "550e8400-e29b-41d4-a716-446655440010";
        let session_key = SessionSlot::worker("TestOrg", "forge", "idle");
        workspace.insert_live_worker(&project_key, fake_spawning_entry("idle", &session_key, true));
        workspace
            .record_worker_row(
                &project_key,
                "idle",
                session_id,
                "charter",
                Some("kick"),
                None,
                false,
                false,
                None,
            )
            .expect("seed the row this arm must keep");

        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        // Do NOT create the JSONL: tag_session will see NotFound every
        // attempt and the retry will exhaust.
        let _path = jsonl_path_for(cfg.path(), cwd.path(), session_id);

        // Use a tight retry budget so the test completes quickly. The
        // _with_config_dir entry point spawns the detached tokio task
        // internally - we override the constants by calling
        // tag_session_with_retry directly here would be cheating; this
        // test verifies the wrapper's classification + transition logic.
        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &session_key,
            session_id,
            &project_key,
            "idle",
            &cwd.path().to_string_lossy(),
            false,
            true,
            true,
            cfg.path(),
        );

        // The detached task takes ~3s (30 x 100ms) to exhaust against an
        // absent JSONL. Wait for the StatusChanged that signals it ran
        // through the deferred-NotFound branch.
        let status_changed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Some(SessionUpdate::WorkerStatusChanged { action, status, .. }) => {
                        if matches!(action, crate::protocol::WorkerStatusAction::StatusChanged) {
                            return Some(status);
                        }
                        if matches!(action, crate::protocol::WorkerStatusAction::Removed) {
                            return None;
                        }
                    }
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await
        .expect("timed out waiting for StatusChanged");

        let status = status_changed.expect("expected StatusChanged, not Removed (rollback)");
        assert_eq!(status.label, "idle");
        assert!(matches!(status.status, forge_primitives::WorkerLiveness::Running));

        // Worker entry should remain in live_workers with needs_tag=true.
        let entries = workspace.list_live_workers(&project_key);
        assert_eq!(entries.len(), 1, "worker must NOT have been rolled back");
        assert_eq!(entries[0].label, "idle");
        assert!(entries[0].needs_tag, "needs_tag stays true so opportunistic retry can fire");
        assert!(matches!(entries[0].status, forge_primitives::WorkerLiveness::Running));
        assert!(
            !workspace.worker_rows_for_project(&project_key).is_empty(),
            "the durable row stays: nothing was rolled back, and the row is what re-spawns this \
             worker after a restart",
        );
    }

    /// Drive a non-NotFound tag failure over a seeded row and live worker,
    /// and return the row that survived it. `needs_tag` and `minted_row`
    /// are the two facts the rollback decides the row on; the malformed
    /// session id is the failure, since no amount of waiting puts a JSONL
    /// on disk that makes that write succeed.
    async fn non_notfound_rollback_leftover_row(
        needs_tag: bool,
        minted_row: bool,
    ) -> Option<crate::store::sessions::SessionRecord> {
        let (workspace, mut rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj-x", "/tmp/proj-x");
        let db_dir = tempdir().expect("db tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        let project_key = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/proj-x")),
        );
        let session_key = SessionSlot::worker("TestOrg", "proj-x", "crashed");
        workspace.insert_live_worker(
            &project_key,
            fake_spawning_entry("crashed", &session_key, needs_tag),
        );
        workspace
            .record_worker_row(
                &project_key,
                "crashed",
                "crashed-uuid",
                "charter",
                Some("kick"),
                None,
                false,
                false,
                None,
            )
            .expect("seed the row the rollback judges");

        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &session_key,
            "not-a-uuid",
            &project_key,
            "crashed",
            &cwd.path().to_string_lossy(),
            false,
            needs_tag,
            minted_row,
            cfg.path(),
        );

        loop {
            let update = rx.recv().await.expect("the rollback emits an update");
            if let SessionUpdate::WorkerStatusChanged { action, status, .. } = update
                && action == crate::protocol::WorkerStatusAction::Removed
            {
                assert_eq!(status.label, "crashed", "the rolled-back worker is the one removed");
                break;
            }
        }
        assert!(
            workspace.list_live_workers(&project_key).is_empty(),
            "the rollback removes the live entry whatever becomes of the row",
        );

        let db = workspace.db.lock();
        crate::store::sessions::get(db.as_ref().expect("db"), "TestOrg", "proj-x", "crashed")
            .expect("read the row the rollback left")
    }

    /// The non-NotFound rollback discards a spawn that minted its own row,
    /// so the row goes with it: left behind, it is a row with no live
    /// worker - which `agents__despawn` cannot clear - and the next boot
    /// re-spawns the worker this arm just rolled back (#1142).
    #[tokio::test]
    async fn a_non_notfound_tag_failure_rolls_back_the_row_with_the_worker() {
        assert!(
            non_notfound_rollback_leftover_row(true, true).await.is_none(),
            "a spawn that wrote the row takes it with it",
        );
    }

    /// A resume and a boot re-spawn are handed a row that pre-existed and
    /// holds the worker's charter, kick and the id being resumed. Taking
    /// it away loses a worker that only failed to write a JSONL tag, and
    /// leaves nothing to resume it from.
    #[tokio::test]
    async fn a_non_notfound_tag_failure_keeps_a_row_this_spawn_did_not_mint() {
        assert_eq!(
            non_notfound_rollback_leftover_row(true, false).await.and_then(|row| row.charter),
            Some("charter".to_owned()),
            "a row this spawn did not write is not this spawn's to delete",
        );
    }

    /// The arm also fires on every `/new` re-tag, over a row whose worker
    /// has already been tagged on disk. That row belongs to the worker,
    /// not to the connection that failed to re-tag it.
    #[tokio::test]
    async fn a_non_notfound_tag_failure_keeps_a_row_the_worker_already_holds() {
        assert_eq!(
            non_notfound_rollback_leftover_row(false, true).await.and_then(|row| row.charter),
            Some("charter".to_owned()),
            "a worker already tagged on disk keeps its row when a later tag write fails",
        );
    }

    /// `apply_worker_tag_or_rollback_with_config_dir`: when the JSONL
    /// exists from the start, the retry succeeds on the first attempt,
    /// the worker transitions to Running, and `needs_tag` is cleared.
    #[tokio::test]
    async fn jsonl_present_clears_needs_tag() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("forge");
        let session_id = "550e8400-e29b-41d4-a716-446655440011";
        let session_key = SessionSlot::worker("TestOrg", "forge", "prompt-driven");
        workspace.insert_live_worker(
            &project_key,
            fake_spawning_entry("prompt-driven", &session_key, true),
        );

        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let path = jsonl_path_for(cfg.path(), cwd.path(), session_id);
        fs::write(&path, "").expect("seed jsonl");

        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &session_key,
            session_id,
            &project_key,
            "prompt-driven",
            &cwd.path().to_string_lossy(),
            false,
            true,
            true,
            cfg.path(),
        );

        // Wait for the StatusChanged emit.
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(update) = rx.recv().await {
                if let SessionUpdate::WorkerStatusChanged { action, .. } = update
                    && matches!(action, crate::protocol::WorkerStatusAction::StatusChanged)
                {
                    break;
                }
            }
        })
        .await
        .expect("StatusChanged emit within budget");

        let entries = workspace.list_live_workers(&project_key);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].needs_tag, "needs_tag cleared on successful tag-write");
        assert!(matches!(entries[0].status, forge_primitives::WorkerLiveness::Running));
        let body = fs::read_to_string(&path).expect("read jsonl");
        assert!(body.contains("\"tag\":\"forge:worker:prompt-driven\""));
    }

    /// `retry_worker_tag_opportunistic_with_config_dir` on a worker
    /// whose `needs_tag` is true: when the JSONL is present, the
    /// retry succeeds, the flag is cleared, and a single
    /// `StatusChanged` event fires.
    #[tokio::test]
    async fn opportunistic_retry_clears_flag_when_jsonl_appears() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("forge");
        let session_id = "550e8400-e29b-41d4-a716-446655440012";
        let session_key = SessionSlot::worker("TestOrg", "forge", "idle");
        // Pre-state: worker is Running but needs_tag=true (it landed
        // here via the deferred-NotFound branch earlier).
        let mut entry = fake_spawning_entry("idle", &session_key, true);
        entry.status = forge_primitives::WorkerLiveness::Running;
        workspace.insert_live_worker(&project_key, entry);

        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let path = jsonl_path_for(cfg.path(), cwd.path(), session_id);
        fs::write(&path, "").expect("seed jsonl (claude has now written one)");

        workspace.retry_worker_tag_opportunistic_with_config_dir(
            &project_key,
            &session_key,
            session_id,
            "idle",
            &cwd.path().to_string_lossy(),
            false,
            cfg.path(),
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(update) = rx.recv().await {
                if let SessionUpdate::WorkerStatusChanged { action, .. } = update
                    && matches!(action, crate::protocol::WorkerStatusAction::StatusChanged)
                {
                    break;
                }
            }
        })
        .await
        .expect("StatusChanged within budget");

        let entries = workspace.list_live_workers(&project_key);
        assert!(!entries[0].needs_tag, "needs_tag cleared on opportunistic retry success");
        let body = fs::read_to_string(&path).expect("read jsonl");
        assert!(body.contains("\"tag\":\"forge:worker:idle\""));
    }

    /// #166 regression: when a worker session hits /new, the new
    /// session_id needs its own tag row written to its JSONL.
    /// `session_task::translate_event` now calls
    /// `apply_worker_tag_or_rollback` on EVERY Connected (not just
    /// the first), so the post-/new JSONL gets tagged in lockstep
    /// with the entry's session_key migration. Without this, the
    /// post-/new transcript carries no `forge:worker` tag, so the boot
    /// scan lists it as one of the project's own sessions.
    ///
    /// Workspace-level test: simulate the /new flow by calling the
    /// tagger twice, swapping the WorkerEntry's occupant between calls.
    /// Verify both session_ids' JSONLs land with the tag. The slot does
    /// not move across `/new` - only the occupant the entry carries
    /// does - so both Connecteds name the one slot.
    #[tokio::test]
    async fn worker_tag_re_applied_after_new_session_rekey() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("forge");

        // Two distinct session_ids: first Connected, then post-/new.
        let session_id_1 = "550e8400-e29b-41d4-a716-446655440021";
        let session_id_2 = "550e8400-e29b-41d4-a716-446655440022";
        let key_1 = SessionSlot::worker("TestOrg", "forge", "reviewer");

        // Seed the WorkerEntry at the slot the worker was spawned
        // under, which `/new` leaves alone.
        workspace.insert_live_worker(&project_key, fake_spawning_entry("reviewer", &key_1, true));

        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let path_1 = jsonl_path_for(cfg.path(), cwd.path(), session_id_1);
        fs::write(&path_1, "").expect("seed first jsonl");

        // First Connected: tag the first session's JSONL.
        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &key_1,
            session_id_1,
            &project_key,
            "reviewer",
            &cwd.path().to_string_lossy(),
            false,
            true,
            true,
            cfg.path(),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(update) = rx.recv().await {
                if let SessionUpdate::WorkerStatusChanged { action, .. } = update
                    && matches!(action, crate::protocol::WorkerStatusAction::StatusChanged)
                {
                    break;
                }
            }
        })
        .await
        .expect("first StatusChanged within budget");
        let body_1 = fs::read_to_string(&path_1).expect("read first jsonl");
        assert!(
            body_1.contains("\"tag\":\"forge:worker:reviewer\""),
            "first Connected tags the first session's JSONL",
        );

        // Simulate `/new`: the slot does not move, only the occupant
        // the registry entry carries does.
        {
            let mut workers = workspace.live_workers.lock();
            for entry in workers.values_mut().flatten() {
                if entry.slot == key_1 {
                    entry.session_id = Some(forge_primitives::SessionId::new(session_id_2));
                }
            }
        }

        // Seed the second session's JSONL as if claude wrote it on
        // the first turn after /new.
        let path_2 = jsonl_path_for(cfg.path(), cwd.path(), session_id_2);
        fs::write(&path_2, "").expect("seed second jsonl");

        // Second Connected (the /new flow). Without #166's fix the
        // tagger was never called; with the fix it fires
        // unconditionally on every Connected.
        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &key_1,
            session_id_2,
            &project_key,
            "reviewer",
            &cwd.path().to_string_lossy(),
            false,
            true,
            true,
            cfg.path(),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(update) = rx.recv().await {
                if let SessionUpdate::WorkerStatusChanged { action, .. } = update
                    && matches!(action, crate::protocol::WorkerStatusAction::StatusChanged)
                {
                    break;
                }
            }
        })
        .await
        .expect("second StatusChanged within budget");

        let body_2 = fs::read_to_string(&path_2).expect("read second jsonl");
        assert!(
            body_2.contains("\"tag\":\"forge:worker:reviewer\""),
            "second Connected (post-/new) tags the new session's JSONL",
        );

        // The first JSONL keeps its tag - re-tagging writes to the
        // new file, not the old one. This guards against accidental
        // first-JSONL overwrite during the re-tag flow.
        let body_1_after = fs::read_to_string(&path_1).expect("read first jsonl");
        assert!(
            body_1_after.contains("\"tag\":\"forge:worker:reviewer\""),
            "first session's tag survives the /new re-tag flow",
        );
    }

    /// #184 regression: when a worker is spawned inside a git repo,
    /// claude's `--worktree <label>` forks the subprocess into
    /// `<repo>/.claude/worktrees/<label>/` and writes the session
    /// JSONL under THAT sanitised path, not the repo root. The
    /// tag-write path must follow the JSONL there.
    ///
    /// Before the fix, `apply_worker_tag_or_rollback_with_config_dir`
    /// passed the repo root to `tag_session_with_retry`, the lookup
    /// missed (different sanitised key), all 30 retries hit NotFound,
    /// and `needs_tag` stayed true forever. On forge restart the
    /// catalog scan found zero tagged worker JSONLs and every worker
    /// spawned fresh.
    ///
    /// Test breaks the existing fixture's "write path = lookup path"
    /// coupling: cwd argument is the repo root, but the JSONL is
    /// seeded at the worktree path's sanitised key.
    #[tokio::test]
    async fn git_repo_worker_tag_lands_under_worktree_path() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("repo");
        let session_id = "550e8400-e29b-41d4-a716-446655440099";
        let session_key = SessionSlot::worker("TestOrg", "repo", "debugger");

        // is_git_repo_at_spawn=true is the production shape that
        // triggers claude's --worktree fork.
        let mut entry = fake_spawning_entry("debugger", &session_key, true);
        entry.is_git_repo_at_spawn = true;
        workspace.insert_live_worker(&project_key, entry);

        let cfg = tempdir().expect("cfg");
        let repo_root = tempdir().expect("repo");
        // JSONL lives at the WORKTREE path (matches claude's behaviour
        // under --worktree): <repo>/.claude/worktrees/<label>/.
        let worktree_path = repo_root.path().join(".claude/worktrees/debugger");
        let path = jsonl_path_for(cfg.path(), &worktree_path, session_id);
        fs::write(&path, "").expect("seed jsonl at worktree path");

        // Caller passes the repo root (NOT the worktree path), matching
        // how forge sources the cwd from the project view. The wrapper
        // must compute the worktree-derived cwd internally when
        // is_git_repo_at_spawn is true.
        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &session_key,
            session_id,
            &project_key,
            "debugger",
            &repo_root.path().to_string_lossy(),
            true,
            true,
            true,
            cfg.path(),
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(update) = rx.recv().await {
                if let SessionUpdate::WorkerStatusChanged { action, .. } = update
                    && matches!(action, crate::protocol::WorkerStatusAction::StatusChanged)
                {
                    break;
                }
            }
        })
        .await
        .expect("StatusChanged within budget");

        let entries = workspace.list_live_workers(&project_key);
        assert_eq!(entries.len(), 1, "worker stays in live_workers after successful tag");
        assert!(
            !entries[0].needs_tag,
            "needs_tag cleared, tag-write found the JSONL at the worktree path"
        );
        let body = fs::read_to_string(&path).expect("read jsonl at worktree path");
        assert!(
            body.contains("\"tag\":\"forge:worker:debugger\""),
            "tag row appended at the worktree-derived JSONL: {body:?}"
        );
    }

    /// #184 regression for the opportunistic-retry path: the
    /// first-turn retry from `handle_deliver_worker_prompt` also
    /// needs to address the worktree-derived JSONL for git-repo
    /// workers. Sibling to `git_repo_worker_tag_lands_under_worktree_path`
    /// Same setup as the apply path, different entry point.
    #[tokio::test]
    async fn git_repo_worker_opportunistic_retry_uses_worktree_path() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("repo");
        let session_id = "550e8400-e29b-41d4-a716-446655440100";
        let session_key = SessionSlot::worker("TestOrg", "repo", "debugger");

        // Pre-state: worker is Running (the apply_* path already ran
        // and exhausted into DeferredNotFound). is_git_repo_at_spawn=true.
        let mut entry = fake_spawning_entry("debugger", &session_key, true);
        entry.status = forge_primitives::WorkerLiveness::Running;
        entry.is_git_repo_at_spawn = true;
        workspace.insert_live_worker(&project_key, entry);

        let cfg = tempdir().expect("cfg");
        let repo_root = tempdir().expect("repo");
        let worktree_path = repo_root.path().join(".claude/worktrees/debugger");
        let path = jsonl_path_for(cfg.path(), &worktree_path, session_id);
        fs::write(&path, "").expect("seed jsonl at worktree path (claude has now written)");

        workspace.retry_worker_tag_opportunistic_with_config_dir(
            &project_key,
            &session_key,
            session_id,
            "debugger",
            &repo_root.path().to_string_lossy(),
            true,
            cfg.path(),
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(update) = rx.recv().await {
                if let SessionUpdate::WorkerStatusChanged { action, .. } = update
                    && matches!(action, crate::protocol::WorkerStatusAction::StatusChanged)
                {
                    break;
                }
            }
        })
        .await
        .expect("StatusChanged within budget");

        let entries = workspace.list_live_workers(&project_key);
        assert!(
            !entries[0].needs_tag,
            "opportunistic retry found the worktree-path JSONL; needs_tag cleared"
        );
        let body = fs::read_to_string(&path).expect("read jsonl at worktree path");
        assert!(
            body.contains("\"tag\":\"forge:worker:debugger\""),
            "tag row appended at worktree path: {body:?}"
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod worker_respawn_tests {
    use super::*;
    use crate::protocol::{Command, SessionChoice, WorkerSpawnReply};

    /// The `demo` project's lead slot, which the fixtures in this module
    /// boot.
    fn lead_slot() -> SessionSlot {
        SessionSlot::lead("TestOrg", "demo")
    }

    /// A live worker entry for the `demo` project, keyed by the slot the
    /// worker's spawn names - the triple `worker_lookup_for_session`
    /// matches on.
    fn worker_entry(label: &str) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.to_owned(),
            charter: "test".to_owned(),
            slot: SessionSlot::worker("TestOrg", "demo", label),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: lead_slot(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// Seed a project into the stub workspace and hand back the
    /// `LoadedProject` a spawn resolves it to.
    fn seed_project_and_return(ws: &Workspace, name: &str, path: &str) -> LoadedProject {
        ws.seed_test_project(name, path);
        ws.find_project_view_by_name(name).expect("seeded project")
    }

    /// The tool names the per-session `forge` MCP server registers for
    /// `kind`, composed exactly as the spawn path composes them. Read
    /// off the server's debug listing rather than substring-searched in
    /// it, so a description naming a tool cannot answer for the
    /// registration.
    fn forge_tool_surface(
        workspace: &Arc<Workspace>,
        kind: crate::mcp::SessionKind,
    ) -> Vec<String> {
        let server = crate::mcp::build_forge_server(
            crate::mcp::ForgeServerFacades {
                workspace: crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(workspace),
                worker: crate::mcp::workers::facade::ProdWorkerFacade::from_arc(workspace),
                browser: crate::mcp::browser::facade::ProdBrowserFacade::from_workspace(workspace),
                review: crate::mcp::review::facade::ProdReviewFacade::from_arc(workspace),
                cron: crate::mcp::cron::facade::ProdCronFacade::from_arc(workspace),
                gotify: crate::mcp::gotify::facade::ProdGotifyFacade::from_arc(workspace),
                slack: crate::mcp::slack::facade::ProdSlackFacade::from_arc(workspace),
                tasks: crate::mcp::tasks::facade::ProdTasksFacade::from_arc(workspace),
                systemone: workspace.systemone.as_ref().map(|client| {
                    crate::mcp::systemone::facade::ProdSystemOneFacade::new(std::sync::Arc::clone(
                        client,
                    ))
                    .into_arc()
                }),
            },
            &crate::mcp::McpFamily::all(),
            SessionSlot::from_str_for_test("caller"),
            kind,
        );
        let debug = format!("{server:?}");
        let (_, tools) = debug.split_once("tools: [").expect("debug lists the tool names");
        let (tools, _) = tools.split_once(']').expect("the tool list is closed");
        tools
            .split(", ")
            .map(|name| name.trim_matches('"').to_owned())
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// The guard refuses a second claim while the first is outstanding.
    /// Holding the claim directly is all this needs - the blocked branch
    /// is unreachable only when claim and release share one call, which
    /// is a property of the callers rather than of the guard.
    #[test]
    fn respawn_guard_refuses_a_second_claim() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("proj");
        assert!(ws.try_claim_respawn(&key), "an unclaimed guard grants");
        assert!(!ws.try_claim_respawn(&key), "a claimed guard refuses");
        ws.release_respawn(&key);
        assert!(ws.try_claim_respawn(&key), "release makes it claimable again");
    }

    /// A worker must not receive the delegation block. It instructs the
    /// reader to call `agents__spawn`, which is lead-only, so a worker
    /// given it would be told to call a tool that refuses it. The lead
    /// half is the control: without it, a helper that did nothing at all
    /// would still satisfy the assertion above. The negative pin keeps
    /// the charter the sole carrier of the delegation default.
    #[test]
    fn only_a_lead_session_gets_the_delegation_block() {
        let mut worker = SessionLaunchSettings::default();
        Workspace::apply_lead_delegation(&mut worker, crate::mcp::SessionKind::Worker);
        assert_eq!(worker.delegation_preamble, None, "a worker gets no delegation block");

        let mut lead = SessionLaunchSettings::default();
        Workspace::apply_lead_delegation(&mut lead, crate::mcp::SessionKind::Lead);
        let preamble = lead.delegation_preamble.expect("a lead does get it");
        assert!(
            preamble.contains("agents__spawn")
                && preamble.contains("always creates the worker in YOUR project")
                && preamble.contains("Workers build; subagents review"),
            "a lead does get it",
        );
        assert!(
            !preamble.contains("doing the work yourself"),
            "the charter, not this block, carries the delegation default",
        );
        // The preamble is lead-facing text, and a lead's launch denies the
        // replaced CLI tools: naming one here sends the lead to a surface
        // it does not have. The charter carries the same test in `spawn`.
        for tool in [
            "CronCreate",
            "CronDelete",
            "CronList",
            "TaskCreate",
            "TaskGet",
            "TaskList",
            "TaskUpdate",
            "SendMessage",
            "ListAgents",
            "Workflow",
            "RemoteTrigger",
        ] {
            assert!(
                !preamble.contains(tool),
                "the delegation preamble must not name the blocked CLI tool {tool}",
            );
        }
    }

    /// The role a spawn carries is the role it gets, and the slot's label
    /// comes from the same value - so a worker cannot be handed a worker's
    /// tool surface AND the lead's address. A worker re-spawned by the
    /// boot resume path was classified as Lead while the key's shape was
    /// the only signal, which hands it the lead-only `agents__*` group; the
    /// caller that knows the row is a worker now says so.
    #[test]
    fn a_worker_spawn_carries_its_label_and_a_worker_tool_surface() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = seed_project_and_return(&ws, "forge", "/tmp/role-worker");
        let role = crate::protocol::SpawnRole::Worker {
            label: "implementer".to_owned(),
            wrote_row: true,
            mcp_families: None,
        };

        let slot = Workspace::slot_for_spawn(&role, &project);
        assert_eq!(slot.label(), "implementer", "the slot names the worker");
        let kind = if slot.is_lead() {
            crate::mcp::SessionKind::Lead
        } else {
            crate::mcp::SessionKind::Worker
        };
        assert!(
            !forge_tool_surface(&ws, kind).contains(&"agents__spawn".to_owned()),
            "and a worker's forge server carries no lead-only verb",
        );
    }

    /// Controls for [`a_worker_spawn_carries_its_label_and_a_worker_tool_surface`]:
    /// without a case that answers Lead, a derivation answering Worker for
    /// everything would satisfy it. The shape cases the old prefix test
    /// read are unreachable here by construction - `slot_for_spawn` takes
    /// no key, so a parse cannot be reintroduced without a signature
    /// change - which is the stronger form of that pin.
    #[test]
    fn a_lead_spawn_carries_no_label() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = seed_project_and_return(&ws, "forge", "/tmp/role-lead");

        let slot = Workspace::slot_for_spawn(&crate::protocol::SpawnRole::Lead, &project);
        assert!(slot.is_lead(), "a lead spawn carries the lead label");
    }

    /// A tool surface for each kind, which is what the role gates: this is
    /// the lead half of the pair above, and it is the control that stops a
    /// derivation answering Worker for everything from satisfying the
    /// worker case while stripping the lead-only verbs from every lead.
    #[test]
    fn a_lead_tool_surface_keeps_its_lead_only_verbs() {
        let (ws, _rx) = Workspace::testing_stub();
        assert!(
            forge_tool_surface(&ws, crate::mcp::SessionKind::Lead)
                .contains(&"agents__spawn".to_owned()),
            "a lead keeps the verbs only a lead may call",
        );
    }

    #[test]
    fn force_new_respawn_dispatches_workers_fresh() {
        // A force-new lead came up fresh, so its workers do too: every
        // one dispatches with resume_existing = None.
        let (workspace, _update_rx) = Workspace::testing_stub();
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        workspace.seed_test_project("data-modules", "/tmp/force-new-data-modules");
        let key = workspace.project_key_for_name("data-modules").expect("seeded project");
        seed_worker_row(&workspace, &key, "steward");
        workspace.enable_test_dispatch_intercept();
        workspace.respawn_workers_for_lead(
            &SessionSlot::from_str_for_test("lead-uuid"),
            key,
            true, // force_new: the workers come up fresh, like their lead
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "force-new still spawns the stored worker");
        if let Command::SpawnWorker { resume_existing, .. } = spawns[0] {
            assert!(resume_existing.is_none(), "force_new => worker spawns fresh (no resume)");
        }
    }

    /// A stub carrying one `proj-x` project (org `TestOrg`, path
    /// `/tmp/proj-x`) over an empty store, plus the key that project
    /// resolves under. The store is what the re-spawn wave reads, so a
    /// test expecting a resume writes the label's row through
    /// `record_session_id`. The tempdir must outlive the caller.
    fn stub_with_worker_store() -> (Arc<Workspace>, ProjectKey, tempfile::TempDir) {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj-x", "/tmp/proj-x");
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let key = ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
            Some("/tmp/proj-x"),
        ));
        (workspace, key, dir)
    }

    /// A `sessions` row for a `proj-x` worker: the shape the re-spawn
    /// wave reads. `session_id` is absent unless a test writes one
    /// through `record_session_id`, which is what makes the row resume
    /// rather than re-deliver its kick.
    fn worker_row(label: &str, kick: Option<&str>) -> crate::store::sessions::SessionRecord {
        crate::store::sessions::SessionRecord {
            org: "TestOrg".to_owned(),
            project: "proj-x".to_owned(),
            label: label.to_owned(),
            session_id: None,
            charter: Some(format!("dynamic charter for {label}")),
            kick: kick.map(str::to_owned),
            resume_kick: None,
            interactive: Some(false),
            is_git_repo: None,
            mcp_families: None,
        }
    }

    /// `proj-x` is a real git repo and `proj-y` is not, so the four
    /// seeded rows below cut across the two axes the skip must not
    /// confuse: whether the ROW says its worker runs in a worktree, and
    /// whether the project path looks like a repo on disk. The two
    /// disagreeing rows are the point - a predicate inferring gitness
    /// from the filesystem gets both of them wrong.
    fn seed_worktree_fixture(
        workspace: &Arc<Workspace>,
    ) -> (ProjectKey, ProjectKey, tempfile::TempDir, tempfile::TempDir) {
        let repo = tempfile::tempdir().expect("git project dir");
        run_git_in(repo.path(), &["init", "-q"]);
        let plain = tempfile::tempdir().expect("non-git project dir");
        workspace.seed_test_project("proj-x", repo.path().to_str().expect("utf8 repo path"));
        workspace.seed_test_project("proj-y", plain.path().to_str().expect("utf8 plain path"));
        // Only `present`'s worktree stands; the other three are gone.
        std::fs::create_dir_all(repo.path().join(".claude").join("worktrees").join("present"))
            .expect("create the surviving worktree");

        let proj_x = workspace.project_key_for_name("proj-x").expect("seeded project");
        let proj_y = workspace.project_key_for_name("proj-y").expect("seeded project");
        let seeded: [(&ProjectKey, &str, bool); 4] = [
            // A git worker whose worktree is gone: the defect.
            (&proj_x, "stranded", true),
            // Its sibling whose worktree stands.
            (&proj_x, "present", true),
            // Runs in the project root, so it has no worktree to lose -
            // even though the project path IS a repo with a `.git`.
            (&proj_x, "rooted", false),
            // Says it runs in a worktree, in a project with no `.git`
            // anywhere. The row decides, not the disk.
            (&proj_y, "ghostworktree", true),
        ];
        for (key, label, is_git) in seeded {
            workspace
                .record_worker_row(
                    key,
                    label,
                    &format!("{label}-uuid"),
                    "c",
                    None,
                    None,
                    false,
                    is_git,
                    None,
                )
                .expect("seed the row this wave re-spawns");
        }
        // A row whose spawn never got an id. The wave re-spawns it FRESH,
        // which runs in the project root and passes `--worktree`, so
        // claude creates the worktree itself and there is nothing missing
        // for it to fail on.
        let guard = workspace.db.lock();
        crate::store::sessions::put(
            guard.as_ref().expect("db installed"),
            &crate::store::sessions::SessionRecord {
                org: "TestOrg".to_owned(),
                project: "proj-x".to_owned(),
                label: "unstarted".to_owned(),
                session_id: None,
                charter: Some("c".to_owned()),
                kick: None,
                resume_kick: None,
                interactive: Some(false),
                is_git_repo: Some(true),
                mcp_families: None,
            },
        )
        .expect("seed the id-less row");
        drop(guard);
        (proj_x, proj_y, repo, plain)
    }

    /// A worker whose worktree is gone is re-spawned on every boot and
    /// fails every time, forever. The wave skips it, and only it.
    #[test]
    fn boot_respawn_skips_a_row_whose_worktree_is_gone() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let db_dir = tempfile::tempdir().expect("db dir");
        workspace.install_db_for_test(
            crate::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        let (proj_x, proj_y, _repo, _plain) = seed_worktree_fixture(&workspace);

        for key in [&proj_x, &proj_y] {
            // The wave reads the rows the store holds, not a hand-made
            // shape, so this also pins that the row carries the flag.
            let rows = workspace.worker_rows_for_project(key);
            workspace.dispatch_worker_respawns(&lead_slot(), key, &rows, false);
        }

        let spawned: Vec<String> = workspace
            .drain_test_dispatch_buffer()
            .into_iter()
            .map(|cmd| match cmd {
                Command::SpawnWorker { label, .. } => label,
                other => panic!("expected SpawnWorker, got {other:?}"),
            })
            .collect();
        assert!(
            !spawned.contains(&"stranded".to_owned()),
            "a worker whose worktree is gone must not be re-spawned - the spawn cannot \
             enter its cwd and would fail on every boot forever; spawned {spawned:?}",
        );
        assert!(
            !spawned.contains(&"ghostworktree".to_owned()),
            "the skip must follow the row's recorded gitness, not a `.git` on disk: \
             this row says worktree and the project path has no repo; spawned {spawned:?}",
        );
        assert!(
            spawned.contains(&"present".to_owned()),
            "a worker whose worktree stands must still re-spawn; spawned {spawned:?}",
        );
        assert!(
            spawned.contains(&"rooted".to_owned()),
            "a worker whose row says it runs in the project root must still re-spawn, \
             even though that project holds a `.git`; spawned {spawned:?}",
        );
        assert!(
            spawned.contains(&"unstarted".to_owned()),
            "a row with no stored id re-spawns FRESH and creates its own worktree, so \
             the missing one must not strand it; spawned {spawned:?}",
        );

        // Skipping is not deleting. The row is the only handle on the
        // worker, so a restored worktree is what brings it back.
        let kept = |key: &ProjectKey| -> Vec<String> {
            workspace.worker_rows_for_project(key).into_iter().map(|r| r.label).collect()
        };
        assert!(
            kept(&proj_x).contains(&"stranded".to_owned())
                && kept(&proj_y).contains(&"ghostworktree".to_owned()),
            "a skipped row must stay in the store, or the restore path this promises is \
             gone; kept {:?} / {:?}",
            kept(&proj_x),
            kept(&proj_y),
        );
    }

    /// The launchpad renders every persisted worker row, so a row the
    /// wave skips would still be offered as a worker - which is the
    /// visible half of the same defect. It must not reach the pane.
    ///
    /// Only the skips are asserted here. Both sites call
    /// `worker_row_can_start`, so an over-skip regression fails the boot
    /// wave's test above; what this one pins is that this site calls the
    /// predicate at all.
    #[test]
    fn launchpad_does_not_offer_a_row_whose_worktree_is_gone() {
        let (workspace, _rx) = Workspace::testing_stub();
        let db_dir = tempfile::tempdir().expect("db dir");
        workspace.install_db_for_test(
            crate::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        let (proj_x, _proj_y, _repo, _plain) = seed_worktree_fixture(&workspace);

        let offered = workspace.worker_labels_by_project();
        let labels = offered.get(&proj_x).cloned().unwrap_or_default();
        assert!(
            !labels.contains(&"stranded".to_owned()),
            "a worker whose worktree is gone must not be offered by the launchpad; \
             offered {labels:?}",
        );
        assert!(
            !labels.contains(&"ghostworktree".to_owned()),
            "the launchpad must read the row's gitness, not the project's filesystem; \
             offered {labels:?}",
        );
    }

    /// The interactive flag rides the subprocess CLI args, so a
    /// re-spawn that dropped it would take `AskUserQuestion` away from
    /// a worker mid-conversation on the first forge restart - and give
    /// it to one that was never meant to have it. The family selection
    /// rides the same row for the same reason.
    #[test]
    fn dispatch_worker_respawns_carries_interactive_from_the_row() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.seed_test_project("proj-x", "/tmp/proj-x");
        let project_key = workspace.project_key_for_name("proj-x").expect("seeded project");
        let mut talkative = worker_row("talkative", None);
        talkative.interactive = Some(true);
        talkative.mcp_families = Some(vec!["cron".to_owned()]);
        let dynamic = vec![talkative, worker_row("quiet", None)];

        workspace.dispatch_worker_respawns(
            &SessionSlot::from_str_for_test("new-lead"),
            &project_key,
            &dynamic,
            false,
        );

        for cmd in workspace.drain_test_dispatch_buffer() {
            let Command::SpawnWorker {
                label, interactive, from_boot_respawn, mcp_families, ..
            } = cmd
            else {
                panic!("expected SpawnWorker");
            };
            assert!(
                from_boot_respawn,
                "a boot re-spawn must stay exempt from the worker cap; flipping this \
                 strands persisted rows on restart with the refusal dropped unheard"
            );
            match label.as_str() {
                "talkative" => {
                    assert!(interactive, "an interactive row re-spawns interactive");
                    assert_eq!(
                        mcp_families,
                        Some(vec!["cron".to_owned()]),
                        "the row's family selection rides the re-spawn",
                    );
                }
                "quiet" => {
                    assert!(!interactive, "a non-interactive row re-spawns non-interactive");
                    assert_eq!(mcp_families, None, "a row naming none re-spawns unnarrowed");
                }
                other => panic!("unexpected label {other}"),
            }
        }
    }

    /// Re-spawn resumes by the id the store holds: a worker whose label
    /// has one resumes and takes the forge restart note as its kick; one
    /// without re-delivers its stored kick.
    #[test]
    fn dispatch_worker_respawns_resumes_or_redelivers_kick() {
        let (workspace, project_key, _dir) = stub_with_worker_store();
        workspace.enable_test_dispatch_intercept();
        let dynamic = vec![
            worker_row("reviewer", Some("original kick")),
            worker_row("scratch", Some("start scratch")),
            worker_row("idle", None),
        ];
        workspace.record_session_id("TestOrg", "proj-x", "reviewer", "reviewer-uuid");

        workspace.dispatch_worker_respawns(
            &SessionSlot::from_str_for_test("new-lead"),
            &project_key,
            &dynamic,
            false,
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 3, "one SpawnWorker per persisted dynamic worker");
        for cmd in dispatched {
            let Command::SpawnWorker {
                label,
                charter,
                resume_existing,
                kick,
                spawned_by,
                project_key: pk,
                ..
            } = cmd
            else {
                panic!("expected SpawnWorker");
            };
            assert_eq!(
                spawned_by,
                SessionSlot::from_str_for_test("new-lead"),
                "re-parented to the current lead's slot",
            );
            assert_eq!(pk, project_key);
            assert_eq!(charter, format!("dynamic charter for {label}"), "charter from the DB row");
            match label.as_str() {
                "reviewer" => {
                    assert_eq!(
                        resume_existing.as_deref(),
                        Some("reviewer-uuid"),
                        "a stored id resumes"
                    );
                    assert_eq!(
                        kick.as_deref(),
                        Some(DYNAMIC_WORKER_RESTART_NOTE),
                        "resume delivers the forge restart note, not the stored kick",
                    );
                }
                "scratch" => {
                    assert!(resume_existing.is_none(), "no stored id -> fresh");
                    assert_eq!(
                        kick.as_deref(),
                        Some("start scratch"),
                        "fresh re-delivers the stored kick"
                    );
                }
                "idle" => {
                    assert!(resume_existing.is_none());
                    assert!(kick.is_none(), "fresh with no stored kick stays kickless");
                }
                other => panic!("unexpected label {other}"),
            }
        }
    }

    /// A row carrying its own `resume_kick` takes that text on resume
    /// instead of the generic restart note. The fresh-spawn path is
    /// untouched by it: no stored id still means the stored kick.
    #[test]
    fn dispatch_worker_respawns_prefers_the_rows_resume_kick() {
        let (workspace, project_key, _dir) = stub_with_worker_store();
        workspace.enable_test_dispatch_intercept();
        let mut steward = worker_row("steward", Some("original kick"));
        steward.resume_kick = Some("Re-read the taste notes, then drain both queues.".to_owned());
        let mut fresh = worker_row("fresh", Some("original kick"));
        fresh.resume_kick = Some("never delivered: this one has no stored id".to_owned());
        let dynamic = vec![steward, fresh];
        workspace.record_session_id("TestOrg", "proj-x", "steward", "steward-uuid");

        workspace.dispatch_worker_respawns(
            &SessionSlot::from_str_for_test("new-lead"),
            &project_key,
            &dynamic,
            false,
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 2);
        for cmd in dispatched {
            let Command::SpawnWorker { label, kick, .. } = cmd else {
                panic!("expected SpawnWorker");
            };
            match label.as_str() {
                "steward" => assert_eq!(
                    kick.as_deref(),
                    Some("Re-read the taste notes, then drain both queues."),
                    "a resuming worker with its own resume_kick gets that, not the generic note",
                ),
                "fresh" => assert_eq!(
                    kick.as_deref(),
                    Some("original kick"),
                    "a fresh re-spawn still takes the stored kick",
                ),
                other => panic!("unexpected label {other}"),
            }
        }
    }

    /// A persisted worker re-spawns on lead reconnect, carrying the
    /// row's own charter.
    #[test]
    fn catalog_scan_respawns_persisted_worker() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let project_path = dir.path().join("catalog-scan-respawn");
        std::fs::create_dir_all(&project_path).expect("create the project dir");
        workspace.seed_test_project("data-modules", &project_path.to_string_lossy());
        let project_key = workspace.project_key_for_name("data-modules").expect("seeded project");
        workspace
            .record_worker_row(
                &project_key,
                "scratch",
                "scratch-test-id",
                "resume the scratch task",
                Some("go"),
                None,
                false,
                false,
                None,
            )
            .expect("seed the persisted worker this test re-spawns");
        workspace.enable_test_dispatch_intercept();

        // No tokio runtime in a plain #[test] -> the sync fallback path.
        workspace.respawn_workers_for_lead(
            &SessionSlot::from_str_for_test("lead-uuid"),
            project_key,
            false,
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "the persisted worker re-spawns on lead reconnect");
        if let Command::SpawnWorker { label, charter, .. } = spawns[0] {
            assert_eq!(label, "scratch");
            assert_eq!(charter, "resume the scratch task", "charter comes from the DB row");
        }
    }

    /// A dynamic worker whose row was deleted (despawn / close) does NOT
    /// re-spawn on the next lead reconnect.
    #[test]
    fn catalog_scan_skips_deleted_dynamic_worker() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        workspace.seed_test_project("data-modules", "/tmp/catalog-scan-deleted");
        let project_key = workspace.project_key_for_name("data-modules").expect("seeded project");
        // Expect rather than discard: a silent write failure would leave
        // nothing to delete and the negative assertion would hold for the
        // wrong reason.
        workspace
            .record_worker_row(
                &project_key,
                "scratch",
                "scratch-test-id",
                "c",
                None,
                None,
                false,
                false,
                None,
            )
            .expect("write the row this test then deletes");
        workspace.delete_worker_row(&project_key, "scratch").expect("delete the row");
        workspace.enable_test_dispatch_intercept();

        workspace.respawn_workers_for_lead(
            &SessionSlot::from_str_for_test("lead-uuid"),
            project_key,
            false,
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::SpawnWorker { .. })),
            "a deleted dynamic worker must not re-spawn",
        );
    }

    /// The id the fixture writes into the label's transcript, which is not
    /// the id its row holds: an assertion on a resolved id says which of
    /// the two answered.
    const TRANSCRIPT_ONLY_ID: &str = "660e8400-e29b-41d4-a716-4466554400aa";

    /// Two fixed seconds for a test that reads the newest transcript:
    /// the later one has to be the test's own write whatever the clock's
    /// granularity is.
    const TRANSCRIPT_OLDER_SECS: u64 = 1_700_000_000;
    const TRANSCRIPT_NEWER_SECS: u64 = 1_700_000_001;

    /// Boot a real workspace over a one-project forge.toml and give the
    /// `steward` label both of the pointers a session can be found by: the
    /// row, which the boot wave reads and which answers the MCP resume
    /// while it is there, and a tagged transcript in the project root's
    /// directory, which the MCP resume falls back to once the row is gone.
    /// The project is not a git repo, so the worker runs in the project
    /// root. Returns the workspace plus the project's key and path, and
    /// the id the row holds.
    ///
    /// Both tempdirs must outlive the caller.
    fn resumable_worker_fixture(
        project: &tempfile::TempDir,
        cfg: &tempfile::TempDir,
    ) -> (Arc<Workspace>, crate::target::ProjectKey, PathBuf, String) {
        let project_path = project.path().to_string_lossy().replace('\\', "/");
        let forge_dir = cfg.path().join("forge");
        std::fs::create_dir_all(&forge_dir).expect("forge dir");
        std::fs::write(
            forge_dir.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "demo"
path = "{project_path}"

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#
            ),
        )
        .expect("write forge.toml");

        // The id the store holds for the label is what the re-spawn wave
        // resumes onto, so the fixture writes that row.
        let session_id = "550e8400-e29b-41d4-a716-446655440099";
        let ws = Arc::new(Workspace::new_for_test(cfg.path().to_owned()).expect("boot"));
        let view = ws.list_projects().into_iter().find(|v| v.name == "demo").expect("project");
        ws.record_worker_row(
            &view.key,
            "steward",
            session_id,
            "mind the queues",
            None,
            None,
            false,
            // `demo` is not a git repo, so the worker runs in the project
            // root and has no worktree the wave must find.
            false,
            None,
        )
        .expect("the row the re-spawn wave resumes onto");
        write_tagged_transcript(cfg, &view.path, TRANSCRIPT_ONLY_ID, "steward");
        ws.enable_test_dispatch_intercept();
        (ws, view.key.clone(), view.path.clone(), session_id.to_owned())
    }

    /// Poll the intercept buffer until a SpawnWorker lands or the
    /// deadline passes; the dispatch happens in a spawned task after an
    /// async catalog scan.
    async fn await_spawn_worker(ws: &Arc<Workspace>) -> Vec<Command> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let dispatched = ws.drain_test_dispatch_buffer();
            if dispatched.iter().any(|c| matches!(c, Command::SpawnWorker { .. })) {
                return dispatched;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Vec::new()
    }

    /// The async arm: under a runtime the wave runs off the event loop
    /// and re-spawns the worker onto the id the store holds for it.
    #[tokio::test]
    async fn respawn_workers_resumes_a_worker_onto_its_stored_id() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, key, _path, session_id) = resumable_worker_fixture(&project, &cfg);

        ws.respawn_workers_for_lead(&lead_slot(), key, false);

        let dispatched = await_spawn_worker(&ws).await;
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "the persisted row dispatches one SpawnWorker");
        let Command::SpawnWorker { label, resume_existing, .. } = spawns[0] else {
            panic!("expected SpawnWorker");
        };
        assert_eq!(label, "steward");
        assert_eq!(
            resume_existing.as_deref(),
            Some(session_id.as_str()),
            "the wave resumes the worker onto the id the store holds",
        );
    }

    /// A despawn must still stop the revival. The transcript outlives the
    /// despawn, so a boot wave that went looking for labels on disk would
    /// bring back a worker the user closed.
    #[tokio::test]
    async fn a_despawned_labels_transcript_revives_nothing_at_boot() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, key, _path, _session_id) = resumable_worker_fixture(&project, &cfg);
        assert!(
            ws.delete_worker_row(&key, "steward").expect("delete the row"),
            "fixture precondition: the despawn took the row",
        );

        ws.respawn_workers_for_lead(&lead_slot(), key, false);

        let dispatched = await_spawn_worker(&ws).await;
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::SpawnWorker { .. })),
            "the tagged transcript the despawn left is not a reason to bring the worker back",
        );
    }

    /// The MCP path is the only one leads spawn through; its
    /// `from_boot_respawn` must stay false or the cap silently stops
    /// governing exactly the spawns the cap exists for.
    #[tokio::test]
    async fn mcp_spawn_carries_from_boot_respawn_false() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, _path, _session_id) = resumable_worker_fixture(&project, &cfg);
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        // The intercept swallows the command and holds its reply sender,
        // so the facade's await only resolves once the buffer is
        // drained below - which is also where the assertion target
        // comes from.
        let spawner = tokio::spawn(async move {
            let _ = facade
                .spawn_worker(
                    &lead_slot(),
                    "reviewer".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    false,
                    None,
                )
                .await;
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let from_boot_respawn = loop {
            let drained = ws.drain_test_dispatch_buffer();
            if let Some(from_boot_respawn) = drained.iter().find_map(|cmd| match cmd {
                Command::SpawnWorker { from_boot_respawn, .. } => Some(*from_boot_respawn),
                _ => None,
            }) {
                break from_boot_respawn;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the MCP spawn never dispatched a SpawnWorker"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        spawner.await.expect("facade task joins");
        assert!(!from_boot_respawn, "the MCP path is cap-governed, never boot-exempt");
    }

    /// The MCP resume-spawn resolves the label's prior session and
    /// threads it into `Command::SpawnWorker.resume_existing` - the exact
    /// argument the boot re-spawn path fills. The row answers while it is
    /// there, so the id it holds is the one that arrives, not the one in
    /// the label's transcript.
    #[tokio::test]
    async fn mcp_spawn_with_resume_session_threads_the_resolved_session() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, _path, session_id) = resumable_worker_fixture(&project, &cfg);
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let spawner = tokio::spawn(async move {
            facade
                .spawn_worker(
                    &lead_slot(),
                    "steward".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    true,
                    None,
                )
                .await
        });
        let resume_existing = poll_spawn_worker_field(&ws, |cmd| match cmd {
            Command::SpawnWorker { label, resume_existing, .. } if label == "steward" => {
                Some(resume_existing.clone())
            }
            _ => None,
        })
        .await
        .expect("the resume spawn dispatched");
        // The drain above dropped the command's reply sender, so the
        // facade's await resolves to a DispatchFailed we do not assert on.
        let _ = spawner.await.expect("facade task joins");

        assert_eq!(
            resume_existing.as_deref(),
            Some(session_id.as_str()),
            "the facade resumes the id the store holds for the label"
        );
    }

    /// A `resume_session` spawn for a label with nothing to resume starts
    /// a new session rather than refusing: the caller asked for old
    /// context and has to be told it did not get it, but a fresh worker
    /// is usually what it wanted anyway.
    #[tokio::test]
    async fn mcp_spawn_resume_without_prior_session_starts_fresh() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, worktree) = git_worker_fixture(&project, &cfg, "never-used");
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let spawner = tokio::spawn(async move {
            facade
                .spawn_worker(
                    &lead_slot(),
                    "never-used".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    true,
                    None,
                )
                .await
        });
        let resume_existing = poll_spawn_worker_field(&ws, |cmd| match cmd {
            Command::SpawnWorker { label, resume_existing, .. } if label == "never-used" => {
                Some(resume_existing.clone())
            }
            _ => None,
        })
        .await
        .expect("a label with no prior session still spawns");
        let _ = spawner.await.expect("facade task joins");

        assert!(resume_existing.is_none(), "there was nothing to resume, so the session is fresh");
        assert!(
            worktree.exists(),
            "the worktree the ensure minted is the directory the fresh session runs in",
        );
    }

    /// A one-commit git repo as the project plus a workspace booted over
    /// a one-project forge.toml naming it. Returns the workspace, the
    /// project's key, and the label's worktree path (not created).
    fn git_worker_fixture(
        project: &tempfile::TempDir,
        cfg: &tempfile::TempDir,
        label: &str,
    ) -> (Arc<Workspace>, crate::target::ProjectKey, PathBuf) {
        let project_path = project.path().to_string_lossy().replace('\\', "/");
        run_git_in(project.path(), &["init", "-q"]);
        run_git_in(project.path(), &["config", "user.email", "t@example.com"]);
        run_git_in(project.path(), &["config", "user.name", "Test"]);
        std::fs::write(project.path().join("README.md"), "seed").expect("write seed");
        run_git_in(project.path(), &["add", "."]);
        run_git_in(project.path(), &["commit", "-q", "-m", "init"]);

        let forge_dir = cfg.path().join("forge");
        std::fs::create_dir_all(&forge_dir).expect("forge dir");
        std::fs::write(
            forge_dir.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "demo"
path = "{project_path}"

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
            ),
        )
        .expect("write forge.toml");

        let ws = Arc::new(Workspace::new_for_test(cfg.path().to_owned()).expect("boot"));
        let key =
            ws.list_projects().into_iter().find(|v| v.name == "demo").expect("project").key.clone();
        let worktree = project.path().join(".claude").join("worktrees").join(label);
        (ws, key, worktree)
    }

    /// The regression this change exists for: a despawn deletes the
    /// store row, and the row used to be the only pointer to the session
    /// the label resumes onto. The transcript survives the despawn.
    #[tokio::test]
    async fn a_despawned_label_resolves_from_its_transcripts() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, key, worktree) = git_worker_fixture(&project, &cfg, "steward");
        std::fs::create_dir_all(&worktree).expect("worktree dir");
        write_tagged_transcript(&cfg, &worktree, "aaaa", "steward");
        let _store = seed_stored_session(&ws, "steward", "bbbb");
        assert!(
            ws.delete_worker_row(&key, "steward").expect("delete the row"),
            "fixture precondition: the despawn took the row the label used to be found by",
        );

        let found = ws
            .resolve_worker_resume_session("TestOrg", "demo", &worktree, "steward")
            .expect("lookup");
        assert_eq!(
            found,
            ResumeTarget::Found("aaaa".to_owned()),
            "the label resolves to its newest transcript, which is what a despawn leaves behind",
        );
    }

    /// The row is authoritative while it exists: a worker already
    /// registered opens from it even where the directory holds a newer
    /// tagged session. The transcripts are the recovery path for a row
    /// that is gone, not the primary.
    #[tokio::test]
    async fn a_registered_labels_row_wins_over_its_transcripts() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, worktree) = git_worker_fixture(&project, &cfg, "steward");
        std::fs::create_dir_all(&worktree).expect("worktree dir");
        write_tagged_transcript(&cfg, &worktree, "aaaa", "steward");
        let _store = seed_stored_session(&ws, "steward", "bbbb");

        let found = ws
            .resolve_worker_resume_session("TestOrg", "demo", &worktree, "steward")
            .expect("lookup");
        assert_eq!(
            found,
            ResumeTarget::Found("bbbb".to_owned()),
            "the row the label is registered under is what it opens as",
        );
    }

    /// A directory that is there but cannot be read is a failure, not an
    /// absence: a caller told to start fresh would be told a resume
    /// happened on an answer the lookup never gave.
    #[tokio::test]
    async fn an_unreadable_transcript_directory_is_a_failure_not_an_absence() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, worktree) = git_worker_fixture(&project, &cfg, "steward");
        std::fs::create_dir_all(&worktree).expect("worktree dir");
        write_tagged_transcript(&cfg, &worktree, "aaaa", "steward");
        let dir = worker_transcript_dir(&cfg, &worktree);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let found = ws.resolve_worker_resume_session("TestOrg", "demo", &worktree, "steward");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod back");

        let error = found.expect_err("a directory that cannot be read is not an absence");
        assert!(
            error.to_string().contains(&dir.to_string_lossy().to_string()),
            "the failure names the directory it could not read: {error}",
        );
    }

    /// Write a `forge:worker:<label>` tagged transcript under the storage
    /// key of `run_dir` - a worktree, or a project root for a worker with
    /// no worktree of its own - computed while that directory exists, the
    /// way claude names the directory at session time.
    fn write_tagged_transcript(
        cfg: &tempfile::TempDir,
        run_dir: &std::path::Path,
        session_id: &str,
        label: &str,
    ) {
        let worktree_str = run_dir.to_string_lossy().replace('\\', "/");
        let jsonl_dir = worker_transcript_dir(cfg, run_dir);
        std::fs::create_dir_all(&jsonl_dir).expect("jsonl dir");
        std::fs::write(
            jsonl_dir.join(format!("{session_id}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{worktree_str}\",\"message\":{{\"content\":\"hi\"}}}}\n\
                 {{\"type\":\"tag\",\"tag\":\"forge:worker:{label}\"}}\n"
            ),
        )
        .expect("write tagged jsonl");
    }

    /// Pin a transcript's mtime to a fixed second, so a test that reads
    /// the newest one does not depend on the clock's granularity.
    fn pin_mtime(cfg: &tempfile::TempDir, run_dir: &std::path::Path, id: &str, secs: u64) {
        let path = worker_transcript_dir(cfg, run_dir).join(format!("{id}.jsonl"));
        let file = std::fs::File::options().write(true).open(&path).expect("open the transcript");
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .expect("pin the transcript's mtime");
    }

    /// The config-dir directory claude writes `run_dir`'s sessions in.
    fn worker_transcript_dir(cfg: &tempfile::TempDir, run_dir: &std::path::Path) -> PathBuf {
        let run_dir_str = run_dir.to_string_lossy().replace('\\', "/");
        let storage_key =
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some(&run_dir_str));
        forge_sdk::projects_dir_for(cfg.path()).join(storage_key)
    }

    /// The attach case is the data-loss guard on a path that still
    /// refuses: the branch predates the spawn and holds the worker's only
    /// copy of its commits, so the rollback removes the worktree it
    /// created and spares the branch.
    #[tokio::test]
    async fn mcp_resume_lookup_failure_keeps_a_pre_existing_branch() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, worktree) = git_worker_fixture(&project, &cfg, "steward");
        let worktree_str = worktree.to_string_lossy().replace('\\', "/");
        run_git_in(
            project.path(),
            &["worktree", "add", "-q", "-b", "worktree-steward", worktree_str.as_str()],
        );
        std::fs::write(worktree.join("work.txt"), "a worker committed here").expect("write work");
        run_git_in(&worktree, &["add", "."]);
        run_git_in(&worktree, &["commit", "-q", "-m", "real work"]);
        write_tagged_transcript(&cfg, &worktree, "550e8400-e29b-41d4-a716-446655440099", "steward");
        let transcripts = worker_transcript_dir(&cfg, &worktree);
        std::fs::set_permissions(&transcripts, std::fs::Permissions::from_mode(0o000))
            .expect("chmod");
        run_git_in(project.path(), &["worktree", "remove", "--force", worktree_str.as_str()]);
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        // The timeout is the assertion for the other direction: a lookup
        // failure that dispatched instead of returning would sit here
        // waiting on a reply nobody sends, which times out the test
        // rather than failing it with this message.
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            facade.spawn_worker(
                &lead_slot(),
                "steward".to_owned(),
                "charter".to_owned(),
                None,
                None,
                false,
                true,
                None,
            ),
        )
        .await
        .expect("a failed lookup returns; it does not dispatch and wait on a reply")
        .expect_err("the label's transcript directory cannot be read");
        std::fs::set_permissions(&transcripts, std::fs::Permissions::from_mode(0o755))
            .expect("chmod back");

        assert!(
            matches!(err, crate::mcp::workers::facade::WorkerSpawnError::ResumeLookupFailed { .. }),
            "an unreadable lookup is reported as a failure, got {err:?}",
        );
        assert!(
            ws.drain_test_dispatch_buffer()
                .iter()
                .all(|c| !matches!(c, Command::SpawnWorker { .. })),
            "a failed lookup spawns nothing",
        );
        assert!(!worktree.exists(), "the attached worktree is rolled back");
        assert!(
            forge_agent::env::worktree::branch_ref_exists(project.path(), "worktree-steward"),
            "the pre-existing branch and its commits survive the refusal"
        );
    }

    /// A refusal from the shared guard (label already live) arrives
    /// after dispatch, so the rollback rides the reply path: the
    /// worktree the ensure step minted is removed and its branch reaped.
    #[tokio::test]
    async fn mcp_label_live_refusal_rolls_back_a_minted_worktree() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, key, worktree) = git_worker_fixture(&project, &cfg, "steward");
        let worktree_str = worktree.to_string_lossy().replace('\\', "/");
        run_git_in(
            project.path(),
            &["worktree", "add", "-q", "-b", "worktree-steward", worktree_str.as_str()],
        );
        write_tagged_transcript(&cfg, &worktree, "550e8400-e29b-41d4-a716-446655440099", "steward");
        run_git_in(project.path(), &["worktree", "remove", "--force", worktree_str.as_str()]);
        run_git_in(project.path(), &["branch", "-D", "worktree-steward"]);
        let _store = seed_stored_session(&ws, "steward", "550e8400-e29b-41d4-a716-446655440099");
        // No dispatch intercept: the label-live refusal must come from
        // the real shared core, which fires before any subprocess spawn.
        ws.insert_live_worker(&key, worker_entry("steward"));
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let err = facade
            .spawn_worker(
                &lead_slot(),
                "steward".to_owned(),
                "charter".to_owned(),
                None,
                None,
                false,
                true,
                None,
            )
            .await
            .expect_err("the label is already live");
        let crate::mcp::workers::facade::WorkerSpawnError::DispatchFailed { message } = err else {
            panic!("the label-live refusal classifies as DispatchFailed, got {err:?}");
        };
        assert!(message.contains("already live"), "the refusal is the label guard: {message}");
        assert!(!worktree.exists(), "the minted worktree is rolled back");
        assert!(
            !forge_agent::env::worktree::branch_ref_exists(project.path(), "worktree-steward"),
            "the minted branch is reaped"
        );
    }

    /// When the recreation itself cannot happen (the label's branch is
    /// checked out in a second worktree, so `git worktree add` refuses),
    /// the lead gets WorktreeCreationFailed and nothing dispatches - a
    /// log-and-continue mutation here would fall through to a scan the
    /// missing worktree makes misleading.
    #[tokio::test]
    async fn mcp_resume_spawn_surfaces_a_failed_worktree_recreation() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, worktree) = git_worker_fixture(&project, &cfg, "steward");
        run_git_in(
            project.path(),
            &["worktree", "add", "-q", "-b", "worktree-steward", "elsewhere"],
        );
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let err = facade
            .spawn_worker(
                &lead_slot(),
                "steward".to_owned(),
                "charter".to_owned(),
                None,
                None,
                false,
                true,
                None,
            )
            .await
            .expect_err("git cannot check the branch out in a second worktree");
        assert!(
            matches!(
                err,
                crate::mcp::workers::facade::WorkerSpawnError::WorktreeCreationFailed { .. }
            ),
            "the recreation failure surfaces typed, got {err:?}"
        );
        assert!(
            ws.drain_test_dispatch_buffer()
                .iter()
                .all(|c| !matches!(c, Command::SpawnWorker { .. })),
            "the failed recreation stops the spawn before dispatch"
        );
        assert!(!worktree.exists(), "the refused add left no worktree behind");
    }

    /// The motivating shape: a git worker despawned (worktree removed),
    /// then re-spawned with `resume_session`. The facade recreates the
    /// worktree before dispatch, because the transcript lives under the
    /// worktree's storage key and the resumed subprocess needs that cwd.
    ///
    /// The project is reached through a symlink so the two spellings of
    /// its worktree path can never agree by accident: the transcript's
    /// dir was named from the real spelling (existing at write time, so
    /// canonicalised), while a scan that runs without the worktree falls
    /// back to the forge.toml spelling. Recreating before the scan is
    /// what makes the two keys meet; reverted, this test fails on every
    /// platform rather than only where the tempdir root is itself a
    /// symlink.
    #[tokio::test]
    async fn mcp_resume_spawn_recreates_a_despawned_worktree() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        run_git_in(project.path(), &["init", "-q"]);
        run_git_in(project.path(), &["config", "user.email", "t@example.com"]);
        run_git_in(project.path(), &["config", "user.name", "Test"]);
        std::fs::write(project.path().join("README.md"), "seed").expect("write seed");
        run_git_in(project.path(), &["add", "."]);
        run_git_in(project.path(), &["commit", "-q", "-m", "init"]);

        let via_link = project.path().join("via-link");
        std::os::unix::fs::symlink(project.path(), &via_link).expect("symlink project root");
        let project_path = via_link.to_string_lossy().replace('\\', "/");

        let forge_dir = cfg.path().join("forge");
        std::fs::create_dir_all(&forge_dir).expect("forge dir");
        std::fs::write(
            forge_dir.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "demo"
path = "{project_path}"

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
            ),
        )
        .expect("write forge.toml");

        let session_id = "550e8400-e29b-41d4-a716-446655440099".to_owned();
        let worktree = project.path().join(".claude").join("worktrees").join("steward");
        let worktree_str = worktree.to_string_lossy().replace('\\', "/");
        run_git_in(
            project.path(),
            &["worktree", "add", "-q", "-b", "worktree-steward", worktree_str.as_str()],
        );
        // The transcript lives under the worktree's storage key in the
        // config dir's projects tree, so it survives the worktree removal.
        let storage_key =
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some(&worktree_str));
        let jsonl_dir = forge_sdk::projects_dir_for(cfg.path()).join(&storage_key);
        std::fs::create_dir_all(&jsonl_dir).expect("jsonl dir");
        std::fs::write(
            jsonl_dir.join(format!("{session_id}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{worktree_str}\",\"message\":{{\"content\":\"hi\"}}}}\n\
                 {{\"type\":\"tag\",\"tag\":\"forge:worker:steward\"}}\n"
            ),
        )
        .expect("write tagged jsonl");
        // Despawn: remove the worktree the way a clean despawn does.
        run_git_in(project.path(), &["worktree", "remove", "--force", worktree_str.as_str()]);
        assert!(!worktree.exists(), "fixture precondition: the worktree is gone");

        let ws = Arc::new(Workspace::new_for_test(cfg.path().to_owned()).expect("boot"));
        let _store = seed_stored_session(&ws, "steward", &session_id);
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let spawner = tokio::spawn(async move {
            facade
                .spawn_worker(
                    &lead_slot(),
                    "steward".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    true,
                    None,
                )
                .await
        });
        let resume_existing = poll_spawn_worker_field(&ws, |cmd| match cmd {
            Command::SpawnWorker { label, resume_existing, .. } if label == "steward" => {
                Some(resume_existing.clone())
            }
            _ => None,
        })
        .await
        .expect("the resume spawn dispatched");
        // Same as above: the drain dropped the reply sender, so the
        // facade's own result is not the assertion target here.
        let _ = spawner.await.expect("facade task joins");

        assert!(worktree.exists(), "the facade recreated the worktree the despawn removed");
        assert_eq!(resume_existing.as_deref(), Some(session_id.as_str()));
    }

    /// The mirror of the recreation case: the row says the worker runs in
    /// the project root, and the project is a repo. The ensure has to read
    /// the ROW, as the spawn does, or it mints a worktree the resumed
    /// session will not run in and nothing will clean up.
    #[tokio::test]
    async fn mcp_resume_does_not_ensure_a_worktree_the_row_does_not_use() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, key, worktree) = git_worker_fixture(&project, &cfg, "steward");
        let session_id = "550e8400-e29b-41d4-a716-446655440099";
        let dir = tempfile::tempdir().expect("tempdir");
        ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("db"));
        ws.record_worker_row(
            &key, "steward", session_id, "charter", None, None, false, false, None,
        )
        .expect("seed the row that disagrees with the disk");
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let spawner = tokio::spawn(async move {
            facade
                .spawn_worker(
                    &lead_slot(),
                    "steward".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    true,
                    None,
                )
                .await
        });
        poll_spawn_worker_field(&ws, |cmd| match cmd {
            Command::SpawnWorker { label, .. } if label == "steward" => Some(()),
            _ => None,
        })
        .await
        .expect("the resume spawn dispatched");
        let _ = spawner.await.expect("facade task joins");

        assert!(
            !worktree.exists(),
            "the ensure must follow the row's recorded gitness, not probe the project: \
             this row runs in the project root, so no worktree belongs here",
        );
    }

    /// The sequence the regression turned on: a worker spawns - the row
    /// the spawn handler writes and the transcript claude writes in the
    /// worktree are what it leaves behind - then it is despawned, row
    /// deleted and worktree gone. The transcript is not the despawn's to
    /// remove, so the label spawns again onto the session it ran under.
    #[tokio::test]
    async fn a_despawned_label_spawns_again_onto_its_session() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, key, worktree) = git_worker_fixture(&project, &cfg, "steward");
        let worktree_str = worktree.to_string_lossy().replace('\\', "/");
        let session_id = "550e8400-e29b-41d4-a716-446655440099";
        run_git_in(
            project.path(),
            &["worktree", "add", "-q", "-b", "worktree-steward", worktree_str.as_str()],
        );
        write_tagged_transcript(&cfg, &worktree, session_id, "steward");
        let _store = seed_stored_session(&ws, "steward", session_id);
        assert!(ws.delete_worker_row(&key, "steward").expect("delete the row"));
        run_git_in(project.path(), &["worktree", "remove", "--force", worktree_str.as_str()]);
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let spawner = tokio::spawn(async move {
            facade
                .spawn_worker(
                    &lead_slot(),
                    "steward".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    true,
                    None,
                )
                .await
        });
        let Command::SpawnWorker { resume_existing, mcp_families, return_to, .. } =
            take_dispatched_spawn_worker(&ws).await
        else {
            panic!("expected the resume to dispatch a SpawnWorker");
        };
        assert_eq!(
            resume_existing.as_deref(),
            Some(session_id),
            "the label resumes the session its transcript still names",
        );
        // The row is gone (that is what the despawn did), so the spawn
        // legitimately carries no selection: every family.
        assert_eq!(mcp_families, None, "no row means no narrowing");
        // Answer the way `handle_spawn_worker` does for a resume; what
        // the assertion below reads is the facade's own mapping of it,
        // which reports a fallback for a resume that found nothing.
        return_to
            .expect("a dispatch off the workspace's own bus carries a reply channel")
            .send(Ok(WorkerSpawnReply {
                session_id: session_id.to_owned(),
                tag: forge_primitives::worker_tag("steward"),
                mcp_families: None,
                rate_limited_account: None,
                durability_warning: None,
                worktree: None,
                session_choice: SessionChoice::Resumed,
            }))
            .expect("the facade is awaiting its reply");
        let reply = spawner.await.expect("facade task joins").expect("the resume is not an error");

        assert_eq!(
            reply.session_choice,
            SessionChoice::Resumed,
            "a resume that found its session reports the resume, not a fallback",
        );
        assert!(worktree.exists(), "and lands back in the worktree the despawn removed");
    }

    /// A worker in a project that is not a git repo runs in the project
    /// root, so its sessions live in the project's own transcript
    /// directory - the one holding every session that ever ran there.
    /// The label's tag is what picks its own out of that, once the row a
    /// despawn deleted is gone.
    #[tokio::test]
    async fn a_despawned_non_git_label_spawns_again_onto_its_session() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, key, project_path, session_id) = resumable_worker_fixture(&project, &cfg);
        write_tagged_transcript(&cfg, &project_path, &session_id, "steward");
        // Newer, and not this label's: the directory is shared, so
        // newest-in-directory would hand the lead's own work to the
        // worker.
        write_tagged_transcript(
            &cfg,
            &project_path,
            "550e8400-e29b-41d4-a716-4466554400ff",
            "lead",
        );
        // The pick is by mtime, and a tie goes to the greater session id.
        // Pin the two, so "the test's is newer" is a fact of the fixture
        // rather than of the filesystem's clock granularity: on a coarse
        // one both writes land in the same tick, and the tie then hands
        // the win to the fixture's `660e...` over the test's `550e...`.
        pin_mtime(&cfg, &project_path, TRANSCRIPT_ONLY_ID, TRANSCRIPT_OLDER_SECS);
        pin_mtime(&cfg, &project_path, &session_id, TRANSCRIPT_NEWER_SECS);
        assert!(ws.delete_worker_row(&key, "steward").expect("delete the row"));
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let spawner = tokio::spawn(async move {
            facade
                .spawn_worker(
                    &lead_slot(),
                    "steward".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    true,
                    None,
                )
                .await
        });
        let resumed = poll_spawn_worker_field(&ws, |cmd| match cmd {
            Command::SpawnWorker { label, resume_existing, .. } if label == "steward" => {
                Some(resume_existing.clone())
            }
            _ => None,
        })
        .await
        .expect("the despawned label spawns again");
        let _ = spawner.await.expect("facade task joins");

        assert_eq!(
            resumed.as_deref(),
            Some(session_id.as_str()),
            "the label resumes its own session, not the newest one in the directory",
        );
    }

    /// The other half: a resume with nothing to resume starts fresh, and
    /// the reply the tool renders says which happened - a caller that
    /// asked for old context is never left believing it got some.
    #[tokio::test]
    async fn a_resume_with_nothing_to_resume_reports_a_fresh_session() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, _worktree) = git_worker_fixture(&project, &cfg, "ghost");
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let spawner = tokio::spawn(async move {
            facade
                .spawn_worker(
                    &lead_slot(),
                    "ghost".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    true,
                    None,
                )
                .await
        });
        let Command::SpawnWorker { resume_existing, return_to, .. } =
            take_dispatched_spawn_worker(&ws).await
        else {
            panic!("expected the fallback to dispatch a SpawnWorker");
        };
        assert!(resume_existing.is_none(), "nothing to resume, so the session is fresh");
        // Answer the way `handle_spawn_worker` does for a spawn carrying
        // no `resume_existing`, which is what a fallback dispatches -
        // whatever flag the caller passed. Whether it asked to resume and
        // found nothing is known only to the facade, which restates it.
        return_to
            .expect("a dispatch off the workspace's own bus carries a reply channel")
            .send(Ok(WorkerSpawnReply {
                session_id: "fresh-session-uuid".into(),
                tag: forge_primitives::worker_tag("ghost"),
                mcp_families: None,
                rate_limited_account: None,
                durability_warning: None,
                worktree: None,
                session_choice: SessionChoice::Fresh,
            }))
            .expect("the facade is awaiting its reply");
        let reply =
            spawner.await.expect("facade task joins").expect("the fallback is not an error");

        assert_eq!(
            reply.session_choice,
            SessionChoice::FreshWithoutPrior,
            "a resume that found nothing reports the fallback, not a plain fresh spawn",
        );
    }

    /// The third outcome, one arm over from the fallback: a spawn that
    /// never asked to resume reports a plain fresh session, so a lead that
    /// set no flag is not told a lookup found nothing.
    #[tokio::test]
    async fn a_spawn_without_the_flag_reports_a_plain_fresh_session() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, _key, _worktree) = git_worker_fixture(&project, &cfg, "ghost");
        ws.enable_test_dispatch_intercept();
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&ws);

        let spawner = tokio::spawn(async move {
            facade
                .spawn_worker(
                    &lead_slot(),
                    "ghost".to_owned(),
                    "charter".to_owned(),
                    None,
                    None,
                    false,
                    false,
                    None,
                )
                .await
        });
        let Command::SpawnWorker { resume_existing, return_to, .. } =
            take_dispatched_spawn_worker(&ws).await
        else {
            panic!("expected the plain spawn to dispatch a SpawnWorker");
        };
        assert!(resume_existing.is_none(), "the flag was not set, so nothing is resumed");
        return_to
            .expect("a dispatch off the workspace's own bus carries a reply channel")
            .send(Ok(WorkerSpawnReply {
                session_id: "fresh-session-uuid".into(),
                tag: forge_primitives::worker_tag("ghost"),
                mcp_families: None,
                rate_limited_account: None,
                durability_warning: None,
                worktree: None,
                session_choice: SessionChoice::Fresh,
            }))
            .expect("the facade is awaiting its reply");
        let reply = spawner.await.expect("facade task joins").expect("the spawn is not an error");

        assert_eq!(
            reply.session_choice,
            SessionChoice::Fresh,
            "a spawn that never asked to resume is not a resume that found nothing",
        );
    }

    /// The first `SpawnWorker` a facade dispatched, taken whole so the
    /// test can answer its reply channel the way the spawn handler would.
    async fn take_dispatched_spawn_worker(ws: &Arc<Workspace>) -> Command {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(cmd) = ws
                .drain_test_dispatch_buffer()
                .into_iter()
                .find(|c| matches!(c, Command::SpawnWorker { .. }))
            {
                return cmd;
            }
            assert!(std::time::Instant::now() < deadline, "no SpawnWorker was dispatched");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    fn run_git_in(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    /// Record `id` as the store's answer for `label` in the `demo`
    /// project. A resume reads the store, so a test that expects one has
    /// to put the id there; `new_for_test` boots without a store, so
    /// this installs one. The tempdir must outlive the caller.
    fn seed_stored_session(ws: &Arc<Workspace>, label: &str, id: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("db"));
        ws.record_session_id("TestOrg", "demo", label, id);
        dir
    }

    /// Poll the intercept buffer until `pick` matches a SpawnWorker or
    /// the deadline passes; the dispatch happens inside the facade's
    /// awaited dispatch and the buffer holds the command until drained.
    async fn poll_spawn_worker_field<T>(
        ws: &Arc<Workspace>,
        pick: impl Fn(&Command) -> Option<T>,
    ) -> Option<T> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let drained = ws.drain_test_dispatch_buffer();
            let picked = drained.iter().find_map(&pick);
            if picked.is_some() {
                return picked;
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// `--new`: the same fixture comes up fresh, so the worker that
    /// WOULD have resumed spawns fresh. Paired with the resume test
    /// deliberately - against a fixture with nothing stored both arms
    /// yield None and the branch is unobservable.
    #[tokio::test]
    async fn force_new_spawns_fresh_the_worker_the_scan_would_have_resumed() {
        let project = tempfile::tempdir().expect("project dir");
        let cfg = tempfile::tempdir().expect("cfg dir");
        let (ws, key, _path, _session_id) = resumable_worker_fixture(&project, &cfg);

        ws.respawn_workers_for_lead(&lead_slot(), key, true);

        let dispatched = await_spawn_worker(&ws).await;
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "force_new still spawns the persisted worker");
        let Command::SpawnWorker { resume_existing, .. } = spawns[0] else {
            panic!("expected SpawnWorker");
        };
        assert!(
            resume_existing.is_none(),
            "force_new skips the scan, so a resumable worker still spawns fresh",
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod async_worker_spawn_failure_tests {
    use super::*;
    use crate::mcp::workers::types::WorkerEntry;
    use forge_agent::client::SpawnFailureKind;

    fn fake_worker(label: &str, worker_key: &str, lead_id: &str, is_git: bool) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: "test".to_owned(),
            slot: SessionSlot::from_str_for_test(worker_key),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Spawning,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::from_str_for_test(lead_id),
            needs_tag: true,
            is_git_repo_at_spawn: is_git,
            diagnostic: None,
            kick: None,
        }
    }

    /// Seed the workspace pool with a stub Agent so the notice
    /// dispatch's `pool.lock().contains_key(&lead_key)` lead-
    /// resolution check passes. Mirrors the `install_fake_session_task`
    /// helper used by the migration tests.
    fn install_lead_in_pool(workspace: &Arc<Workspace>, lead_id: &str) -> SessionSlot {
        let key = SessionSlot::from_str_for_test(lead_id);
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        workspace.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        key
    }

    /// Pins the lookup predicate the Connected catalog-mirror guard (in
    /// session_task) depends on: worker_lookup_for_session keyed by the
    /// worker's own id resolves a seeded worker (so its tag-less mirror
    /// is skipped) and not a lead/regular session. The guard's
    /// end-to-end skip is exercised in production.
    #[test]
    fn worker_lookup_drives_catalog_mirror_skip() {
        let (workspace, _rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("proj-x");
        let worker_key = "worker-uuid";
        workspace.insert_live_worker(
            &project_key,
            fake_worker("reviewer", worker_key, "lead-uuid", false),
        );
        assert!(
            workspace
                .worker_lookup_for_session(&SessionSlot::from_str_for_test(worker_key))
                .is_some(),
            "seeded worker must be detected so its tag-less catalog mirror is skipped"
        );
        assert!(
            workspace
                .worker_lookup_for_session(&SessionSlot::from_str_for_test("lead-uuid"))
                .is_none(),
            "the lead is not a live worker, so it is still mirrored into the catalog"
        );
    }

    /// #146: async worktree-creation failure → notice envelope
    /// dispatched to the lead's chat AND the WorkerEntry rolled
    /// back. Verifies both effects in one go.
    #[tokio::test]
    async fn async_failure_with_worktree_classification_dispatches_notice_and_rolls_back() {
        let (workspace, mut update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();

        let project_key = ProjectKey::new("proj-x");
        let worker_key = "worker-uuid";
        let lead_id = "lead-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", worker_key, lead_id, true));
        let lead_key = install_lead_in_pool(&workspace, lead_id);

        let handled = workspace.handle_async_worker_spawn_failure(
            &SessionSlot::from_str_for_test(worker_key),
            "fatal: 'reviewer' is already used by worktree at /a/b/c",
            SpawnFailureKind::Unclassified,
        );
        assert!(handled, "async worker failure path must consume the failure");

        // Notice dispatched via Command::Prompt to the lead's key.
        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> = dispatched
            .iter()
            .filter(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. }))
            .collect();
        assert_eq!(prompts.len(), 1, "exactly one WorkerSpawnFailedNotice envelope");
        if let Command::Prompt { key, text, .. } | Command::PromptUnder { key, text, .. } =
            prompts[0]
        {
            assert_eq!(*key, lead_key, "notice targets the lead session id");
            assert!(text.starts_with("[Worker 'reviewer' spawn failed"));
            assert!(text.contains("already used by worktree"));
        }
        // And the echo a view draws: the CLI does not paint a prompt it was
        // handed on stdin, so without this the page shows nothing at all.
        let echoed = std::iter::from_fn(|| update_rx.try_recv().ok()).any(|update| {
            matches!(
                update,
                SessionUpdate::PeerEnvelopeAppended { key, wrapped, .. }
                    if key == lead_key
                        && matches!(wrapped.kind, WrappedKind::WorkerSpawnFailedNotice)
            )
        });
        assert!(echoed, "the notice reaches the chat as the update a view draws");

        // WorkerEntry rolled back: live_workers is empty.
        assert!(
            workspace.list_live_workers(&project_key).is_empty(),
            "WorkerEntry removed on async failure",
        );
    }

    /// #146 + #245 Layer C: async failure with a non-worktree-classified
    /// message must NOT dispatch a lead-notice; the entry transitions to
    /// `WorkerLiveness::Failed` with the message as diagnostic, so the
    /// user sees the failure surfaced on the row rather than the worker
    /// silently vanishing.
    #[tokio::test]
    async fn async_failure_without_worktree_classification_transitions_to_failed() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();

        let project_key = ProjectKey::new("proj-x");
        let worker_key = "worker-uuid";
        let lead_id = "lead-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", worker_key, lead_id, true));
        install_lead_in_pool(&workspace, lead_id);

        let handled = workspace.handle_async_worker_spawn_failure(
            &SessionSlot::from_str_for_test(worker_key),
            "agent spawn failed: subprocess exited with code 2",
            SpawnFailureKind::Unclassified,
        );
        assert!(handled);

        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> = dispatched
            .iter()
            .filter(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. }))
            .collect();
        assert!(prompts.is_empty(), "non-worktree classifier outcome must NOT dispatch a notice");
        let entries = workspace.list_live_workers(&project_key);
        assert_eq!(entries.len(), 1, "non-worktree failure keeps the entry visible");
        assert!(
            matches!(entries[0].status, forge_primitives::WorkerLiveness::Failed),
            "non-worktree failure transitions to Failed; got {:?}",
            entries[0].status,
        );
        assert_eq!(
            entries[0].diagnostic.as_deref(),
            Some("agent spawn failed: subprocess exited with code 2"),
            "diagnostic captures the ConnectionFailed message",
        );
    }

    /// #146: non-worker session_key (e.g. a lead failure) returns
    /// false and changes nothing - the caller's existing
    /// ConnectionFailed flow proceeds unchanged.
    #[tokio::test]
    async fn async_failure_on_non_worker_session_is_no_op() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let unknown = SessionSlot::from_str_for_test("not-a-worker");
        let handled = workspace.handle_async_worker_spawn_failure(
            &unknown,
            "some unrelated error",
            SpawnFailureKind::Unclassified,
        );
        assert!(!handled);
        assert!(workspace.drain_test_dispatch_buffer().is_empty());
    }

    /// #146: double-fire safety - a second ConnectionFailed for the
    /// same worker (after the first call removed its WorkerEntry on
    /// the WORKTREE-classified path) must be a no-op rather than
    /// re-dispatching the notice.
    ///
    /// The message used here MUST classify as worktree-creation
    /// failure - that's the path that still removes the entry under
    /// #245 Layer C. The non-worktree path transitions to Failed
    /// instead of removing, so it wouldn't exercise the
    /// double-fire-after-removal semantics this test is pinning.
    /// `classify_worker_spawn_failure` validates the predicate
    /// upfront so a future rewording that breaks the contract
    /// surfaces here instead of as a confusing failure further down.
    #[tokio::test]
    async fn async_failure_double_fire_is_no_op() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();

        let project_key = ProjectKey::new("proj-x");
        let worker_key = "worker-uuid";
        let lead_id = "lead-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", worker_key, lead_id, true));
        install_lead_in_pool(&workspace, lead_id);

        let session_key = SessionSlot::from_str_for_test(worker_key);
        let worktree_msg = "fatal: 'reviewer' is already used by worktree at /a";
        // Pin the test's premise: this message must classify as
        // WorktreeCreationFailed so we exercise the remove-then-no-op
        // path. If a future change to the classifier breaks this
        // assumption, the test below would change shape (transition-
        // to-Failed isn't a no-op on re-fire).
        assert!(
            matches!(
                crate::mcp::workers::facade::classify_worker_spawn_failure(
                    worktree_msg,
                    true,
                    SpawnFailureKind::Unclassified,
                ),
                crate::mcp::workers::facade::WorkerSpawnError::WorktreeCreationFailed { .. },
            ),
            "test fixture must classify as worktree failure to exercise the removal path",
        );

        assert!(workspace.handle_async_worker_spawn_failure(
            &session_key,
            worktree_msg,
            SpawnFailureKind::Unclassified
        ));
        let _ = workspace.drain_test_dispatch_buffer();

        // Second call: WorkerEntry already gone, returns false, no
        // new dispatch.
        assert!(!workspace.handle_async_worker_spawn_failure(
            &session_key,
            worktree_msg,
            SpawnFailureKind::Unclassified
        ));
        assert!(workspace.drain_test_dispatch_buffer().is_empty());
    }

    fn install_db(workspace: &Arc<Workspace>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        workspace
            .install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("db"));
        dir
    }

    /// The labels with a persisted row in `project_key`. The store is
    /// keyed by `(org, name, label)`, so the key has to resolve before
    /// the rows can be read.
    fn persisted_labels(workspace: &Arc<Workspace>, project_key: &ProjectKey) -> Vec<String> {
        let project = workspace.project_for_key(project_key).expect("seeded project");
        let guard = workspace.db.lock();
        crate::store::sessions::list_for_project(
            guard.as_ref().expect("db"),
            &project.org,
            &project.name,
        )
        .expect("list")
        .into_iter()
        .map(|w| w.label)
        .collect()
    }

    /// #2: a worktree-creation failure is a hard removal (the worker
    /// never started), so it deletes the persisted dynamic-worker row -
    /// otherwise the row zombie-re-spawns every restart despite a
    /// visibly-failed spawn. Also mirrors the tag-rollback arm's
    /// `release_session`: the pool entry + command sender must not leak
    /// per failed fresh spawn.
    #[tokio::test]
    async fn worktree_failure_deletes_persisted_dynamic_worker_row() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let _dir = install_db(&workspace);
        workspace.enable_test_dispatch_intercept();

        workspace.seed_test_project("proj-x", "/tmp/worktree-failure-proj");
        let project_key = workspace.project_key_for_name("proj-x").expect("seeded project");
        let worker_key = "worker-uuid";
        let lead_id = "lead-uuid";
        seed_worker_row(&workspace, &project_key, "reviewer");
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", worker_key, lead_id, true));
        install_lead_in_pool(&workspace, lead_id);

        let session_key = SessionSlot::from_str_for_test(worker_key);
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        workspace.pool.lock().insert(
            session_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        workspace.command_senders.lock().insert(session_key.clone(), cmd_tx);

        let worktree_msg = "fatal: 'reviewer' is already used by worktree at /a";
        assert!(workspace.handle_async_worker_spawn_failure(
            &session_key,
            worktree_msg,
            SpawnFailureKind::Unclassified
        ));

        assert!(
            persisted_labels(&workspace, &project_key).is_empty(),
            "worktree-failure hard removal deletes the persisted row",
        );
        assert!(
            !workspace.pool.lock().contains_key(&session_key)
                && !workspace.command_senders.lock().contains_key(&session_key),
            "the failed spawn's session registrations are released",
        );
    }

    /// A worktree-creation failure never got a worktree, so the
    /// `Removed` event the close toast is built from must not report one
    /// intact - `Intact` is the arm that names a path, and there is
    /// nothing at that path to preserve. The worker is a GIT worker,
    /// the only case that could report `Intact` at all.
    #[tokio::test]
    async fn worktree_creation_failure_reports_no_worktree_to_preserve() {
        let (workspace, mut update_rx) = Workspace::testing_stub();

        let project_key = ProjectKey::new("proj-x");
        let worker_key = "worker-uuid";
        let lead_id = "lead-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", worker_key, lead_id, true));
        install_lead_in_pool(&workspace, lead_id);

        let session_key = SessionSlot::from_str_for_test(worker_key);
        let worktree_msg = "Error creating worktree: failed to resolve base branch";
        // The call below returns true on either classifier outcome, so
        // only this pins which path ran: the other transitions to
        // Failed and emits no Removed event at all.
        assert!(
            matches!(
                crate::mcp::workers::facade::classify_worker_spawn_failure(
                    worktree_msg,
                    true,
                    SpawnFailureKind::Unclassified,
                ),
                crate::mcp::workers::facade::WorkerSpawnError::WorktreeCreationFailed { .. },
            ),
            "the fixture must drive a real worktree-creation failure",
        );
        assert!(workspace.handle_async_worker_spawn_failure(
            &session_key,
            worktree_msg,
            SpawnFailureKind::Unclassified
        ));

        let mut dispositions = Vec::new();
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::WorkerStatusChanged { action, worktree, .. } = update
                && action == crate::protocol::WorkerStatusAction::Removed
            {
                dispositions.push(worktree);
            }
        }
        assert_eq!(
            dispositions,
            vec![crate::protocol::WorktreeDisposition::Absent],
            "a worktree that failed to be created must not be reported preserved",
        );
    }

    /// #2: a non-worktree failure transitions the worker to Failed
    /// (visible) and KEEPS its row, so it re-spawns on the next restart
    /// to recover or re-fail visibly.
    #[tokio::test]
    async fn transition_to_failed_keeps_persisted_dynamic_worker_row() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let _dir = install_db(&workspace);

        workspace.seed_test_project("proj-x", "/tmp/transition-failed-proj");
        let project_key = workspace.project_key_for_name("proj-x").expect("seeded project");
        let worker_key = "worker-uuid";
        seed_worker_row(&workspace, &project_key, "reviewer");
        // Non-git worker + a generic message classifies as DispatchFailed,
        // driving the transition-to-Failed (visible) path.
        workspace.insert_live_worker(
            &project_key,
            fake_worker("reviewer", worker_key, "lead-uuid", false),
        );

        let session_key = SessionSlot::from_str_for_test(worker_key);
        assert!(workspace.handle_async_worker_spawn_failure(
            &session_key,
            "subprocess exited with code 2",
            SpawnFailureKind::Unclassified,
        ));

        assert_eq!(
            persisted_labels(&workspace, &project_key),
            vec!["reviewer".to_owned()],
            "a Failed-but-visible worker keeps its row for re-spawn",
        );
    }

    /// #245 Layer C test gap 12 + 11: direct unit coverage for
    /// [`transition_worker_to_failed`].
    ///
    /// Covers:
    /// - First call flips status + records diagnostic
    /// - Second call with identical diagnostic is a no-op (no
    ///   extra event emission)
    /// - Second call with a NEW diagnostic re-emits + records new
    ///   diagnostic
    /// - needs_tag is cleared on transition
    #[tokio::test]
    async fn transition_worker_to_failed_idempotent_for_identical_diagnostic() {
        let (workspace, mut update_rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("proj-x");
        let worker_key = "worker-uuid";
        let session_key = SessionSlot::from_str_for_test(worker_key);
        // Worker starts Spawning + needs_tag = true (mirrors
        // fresh-spawn state pre-Connected).
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", worker_key, "lead", true));

        // First call: flips to Failed, records diagnostic, clears
        // needs_tag, emits WorkerStatusChanged.
        transition_worker_to_failed(
            &workspace,
            &project_key,
            &session_key,
            Some("spawn failed".to_owned()),
        );

        let entries = workspace.list_live_workers(&project_key);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, forge_primitives::WorkerLiveness::Failed);
        assert_eq!(entries[0].diagnostic.as_deref(), Some("spawn failed"));
        assert!(!entries[0].needs_tag, "needs_tag must be cleared on Failed transition");

        // Drain the first event so the next assertion is unambiguous.
        let _ = update_rx.try_recv().expect("first transition emitted event");

        // Second call with identical diagnostic: no-op, no new event.
        transition_worker_to_failed(
            &workspace,
            &project_key,
            &session_key,
            Some("spawn failed".to_owned()),
        );
        let second_event = update_rx.try_recv();
        assert!(
            second_event.is_err(),
            "identical diagnostic re-fire must NOT emit a new event; got {second_event:?}",
        );

        // Third call with NEW diagnostic: re-emits + records new text.
        transition_worker_to_failed(
            &workspace,
            &project_key,
            &session_key,
            Some("more specific reason".to_owned()),
        );
        let third_event = update_rx.try_recv();
        assert!(third_event.is_ok(), "fresh diagnostic must re-emit");
        let entries = workspace.list_live_workers(&project_key);
        assert_eq!(entries[0].diagnostic.as_deref(), Some("more specific reason"));
    }

    /// handle_async_worker_spawn_failure drops whatever was parked for the
    /// worker whose spawn died, and the message in it is acknowledged back
    /// to its sender: a spawn that never connected has no session left to
    /// deliver it to, and nothing re-delivers it later.
    #[tokio::test]
    async fn async_worker_spawn_failure_acknowledges_the_parked_message() {
        use crate::mcp::peers::types::{MessageId, WrappedKind, WrappedPrompt};
        let (workspace, mut update_rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("proj-x");
        let worker_key = "builder-uuid";
        let session_key = SessionSlot::from_str_for_test(worker_key);
        workspace
            .insert_live_worker(&project_key, fake_worker("builder", worker_key, "lead", true));
        workspace.enable_test_dispatch_intercept();

        let sender = SessionSlot::from_str_for_test("sender-proj");
        workspace.park_peer_prompt(
            &session_key,
            &sender,
            WrappedPrompt {
                id: MessageId::mint(),
                kind: WrappedKind::Message,
                sender_name: "forge".to_owned(),
                sender_org: "Default".to_owned(),
                body: "ready?".to_owned(),
            },
        );

        workspace.handle_async_worker_spawn_failure(
            &session_key,
            "resume failed: boom",
            SpawnFailureKind::Unclassified,
        );

        let mut echo = None;
        while let Ok(update) = update_rx.try_recv() {
            if let SessionUpdate::PeerEnvelopeAppended { key, wrapped, .. } = update {
                echo = Some((key, wrapped));
            }
        }
        let (key, notice) = echo.expect("the parked message is acknowledged to its sender");
        assert_eq!(key, sender, "on the sender's slot");
        assert!(
            matches!(notice.kind, WrappedKind::DeliveryFailureNotice),
            "as a delivery failure, not a peer message"
        );
        assert!(
            workspace.take_parked_for_slot(&session_key).peer.is_empty(),
            "and the bucket is gone, so nothing re-delivers it"
        );
    }

    /// #245 Layer C test gap 11: a worker that flipped to Failed
    /// then transitions back to Running (e.g. a successful resume
    /// after the user fixed the underlying problem) must clear the
    /// stale diagnostic field. Without this, the Projects pane
    /// would render a healthy Running worker with a phantom
    /// failure sub-row underneath.
    #[tokio::test]
    async fn transition_worker_to_running_clears_prior_failed_diagnostic() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("proj-x");
        let worker_key = "worker-uuid";
        let session_key = SessionSlot::from_str_for_test(worker_key);
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", worker_key, "lead", true));

        // Flip to Failed with a diagnostic.
        transition_worker_to_failed(
            &workspace,
            &project_key,
            &session_key,
            Some("connection refused".to_owned()),
        );
        assert_eq!(
            workspace.list_live_workers(&project_key)[0].diagnostic.as_deref(),
            Some("connection refused"),
        );

        // Transition back to Running. Diagnostic must clear.
        transition_worker_to_running(
            &workspace,
            &project_key,
            &session_key,
            TagWriteResult::Succeeded,
        );
        let entry = &workspace.list_live_workers(&project_key)[0];
        assert_eq!(entry.status, forge_primitives::WorkerLiveness::Running);
        assert!(
            entry.diagnostic.is_none(),
            "diagnostic must clear when worker transitions back to Running; got {:?}",
            entry.diagnostic,
        );
    }

    /// #146: lead-session-gone path - the worker was spawned by a
    /// lead session that has since been released (e.g. /new flow).
    /// Notice dispatch is skipped (warn-logged) but the WorkerEntry
    /// still rolls back.
    #[tokio::test]
    async fn async_failure_when_lead_session_gone_drops_notice_but_rolls_back() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();

        let project_key = ProjectKey::new("proj-x");
        let worker_key = "worker-uuid";
        let lead_id = "lead-gone-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", worker_key, lead_id, true));
        // DELIBERATELY skip install_lead_in_pool - lead is "gone".

        let handled = workspace.handle_async_worker_spawn_failure(
            &SessionSlot::from_str_for_test(worker_key),
            "fatal: 'reviewer' is already used by worktree at /a",
            SpawnFailureKind::Unclassified,
        );
        assert!(handled, "still consumes the failure even when lead is gone");

        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> = dispatched
            .iter()
            .filter(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. }))
            .collect();
        assert!(prompts.is_empty(), "no notice dispatched when lead session is gone");
        assert!(
            workspace.list_live_workers(&project_key).is_empty(),
            "WorkerEntry still rolls back even when notice is dropped",
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod git_scan_cwd_tests {
    use super::*;
    use crate::mcp::workers::types::WorkerEntry;
    use forge_primitives::WorkerLiveness;
    use std::time::SystemTime;

    fn worker_entry(label: &str, session_key: &SessionSlot, is_git: bool) -> WorkerEntry {
        WorkerEntry {
            label: label.into(),
            charter: "test charter".into(),
            slot: session_key.clone(),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
            needs_tag: false,
            is_git_repo_at_spawn: is_git,
            diagnostic: None,
            kick: None,
        }
    }

    /// Seed a project + a worker for it. Returns the project_root
    /// path the project_key derives from, and the worker's session
    /// key for the caller to drive `git_scan_cwd_for_session`.
    fn seed_project_and_worker(
        ws: &Arc<Workspace>,
        project_name: &str,
        project_root: &str,
        worker_label: &str,
        worker_session: &str,
        is_git: bool,
    ) -> (std::path::PathBuf, SessionSlot) {
        ws.seed_test_project(project_name, project_root);
        let project_key = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some(project_root)),
        );
        let session_key = SessionSlot::from_str_for_test(worker_session);
        ws.insert_live_worker(&project_key, worker_entry(worker_label, &session_key, is_git));
        (std::path::PathBuf::from(project_root), session_key)
    }

    #[test]
    fn git_scan_cwd_resolves_worktree_when_cwd_raw_is_project_root() {
        // Fresh-spawn path: `cwd_raw` is the project root (the value
        // claude sends in `AgentEvent::Connected.cwd` before it
        // chdirs into the worktree). The function must compose
        // `<project_root>/.claude/worktrees/<label>` for git workers.
        let (ws, _rx) = Workspace::testing_stub();
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "forge",
            "/tmp/test-forge-fresh",
            "implementer",
            "worker-uuid-fresh",
            true,
        );
        let resolved = ws.git_scan_cwd_for_session(&session_key, &project_root);
        assert_eq!(
            resolved,
            project_root.join(".claude/worktrees").join("implementer"),
            "fresh-spawn cwd_raw must resolve to the worktree path"
        );
    }

    #[test]
    fn git_scan_cwd_resolves_worktree_when_cwd_raw_is_already_worktree_path() {
        // Resumed-worker path: `cwd_raw` is already
        // `<project_root>/.claude/worktrees/<label>` because the
        // catalog row was written after claude chdir'd. Old behavior
        // composed `worker_tag_dir(cwd_raw, label, true)` and doubled
        // the suffix - new behavior anchors on the project_key, so
        // both inputs converge on the same final path.
        let (ws, _rx) = Workspace::testing_stub();
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "forge",
            "/tmp/test-forge-resume",
            "implementer",
            "worker-uuid-resume",
            true,
        );
        let already_worktree = project_root.join(".claude/worktrees").join("implementer");
        let resolved = ws.git_scan_cwd_for_session(&session_key, &already_worktree);
        assert_eq!(
            resolved, already_worktree,
            "resume cwd_raw must NOT double the worktree suffix"
        );
    }

    #[test]
    fn git_scan_cwd_returns_cwd_unchanged_for_lead_session() {
        // Non-worker sessions take the fall-through branch and the
        // raw cwd survives unchanged. No `live_workers` entry exists
        // for the lead.
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("forge", "/tmp/test-forge-lead");
        let lead_key = SessionSlot::from_str_for_test("lead-uuid");
        let lead_cwd = std::path::PathBuf::from("/tmp/test-forge-lead");
        let resolved = ws.git_scan_cwd_for_session(&lead_key, &lead_cwd);
        assert_eq!(resolved, lead_cwd, "lead sessions must get cwd_raw unchanged");
    }

    #[test]
    fn git_scan_cwd_returns_cwd_unchanged_for_non_git_worker() {
        // Non-git workers don't have a worktree fork; they run in
        // the project root itself. Returning cwd_raw unchanged
        // matches the pre-fix behavior for this case.
        let (ws, _rx) = Workspace::testing_stub();
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "forge",
            "/tmp/test-forge-nongit",
            "researcher",
            "worker-uuid-nongit",
            false,
        );
        let resolved = ws.git_scan_cwd_for_session(&session_key, &project_root);
        assert_eq!(resolved, project_root, "non-git worker must use cwd_raw unchanged");
    }

    // ---------------------------------------------------------------
    // #245 Layer B: resume_cwd_for_slot falls back to the owning
    // worker's project_root when the catalog has no recorded cwd.
    // Without this, claude --resume inherits the forge binary's
    // process cwd and derives the JSONL location against the wrong
    // git root (the bug documented in #245).
    // ---------------------------------------------------------------

    /// A worker's listing is read from its worktree, and a lead has none of
    /// its own. A fresh git worker launches in the project root while its
    /// transcripts land in the worktree's project dir, so a listing read
    /// from the launching cwd finds none of the worker's own sessions.
    ///
    /// **The project is loaded from a forge.toml fixture, not the test
    /// overlay**, which is what makes the lead half bite: the overlay is
    /// consulted by `project_for_key` and not by the cwd lookup, so a lead
    /// seeded there resolves to nothing and the `is_lead` guard could be
    /// deleted with the assertion still passing. Loaded, the lead resolves
    /// to its project root and only the guard answers `None`.
    #[test]
    fn worker_listing_is_the_workers_worktree_and_a_lead_has_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("listing-proj");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            format!(
                r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "listing-proj"
path = "{}"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
                root.display()
            ),
        )
        .expect("write forge.toml");
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        let view = ws
            .list_projects()
            .into_iter()
            .find(|view| view.name == "listing-proj")
            .expect("fixture project");
        let lead = SessionSlot::lead(&view.org, &view.name);
        assert_eq!(
            ws.cwd_for_session(&lead).as_deref(),
            view.path.to_str(),
            "the fixture's lead must resolve, or its half of this test proves nothing",
        );
        assert!(
            ws.worker_listing_for(&lead).is_none(),
            "a lead lists from the cwd it launches in, so it has no listing to carry",
        );

        // The slot's label is the worker's label, which is what its tag
        // carries: the two are one string in production.
        let worker = SessionSlot::worker(&view.org, &view.name, "reviewer");
        ws.insert_live_worker(&view.key, worker_entry("reviewer", &worker, true));

        let listing = ws.worker_listing_for(&worker).expect("a worker has a listing of its own");
        assert_eq!(
            listing.label, "reviewer",
            "the listing carries the label the worker's transcripts do",
        );
        assert_eq!(
            listing.dir,
            view.path.join(".claude/worktrees/reviewer"),
            "the listing must be read from the worker's worktree, not the cwd it launches in",
        );
    }

    #[test]
    fn resume_cwd_for_slot_returns_worktree_for_git_worker_with_no_catalog_cwd() {
        // Git-repo worker (the data-modules babysitter / librarian
        // case from #245). Layer B composes the worker's worktree
        // path so claude resolves the JSONL on the first try -
        // passing just the project root would make claude look under
        // the wrong sanitised dir and surface "No conversation
        // found".
        let (ws, _rx) = Workspace::testing_stub();
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "data-modules",
            "/tmp/test-data-modules",
            "babysitter",
            "worker-uuid-hub",
            true,
        );
        let resolved = ws.resume_cwd_for_slot(&session_key);
        assert_eq!(
            resolved,
            project_root.join(".claude/worktrees/babysitter").to_string_lossy(),
            "git-repo worker resume cwd must compose the worktree path via worker_tag_dir",
        );
    }

    #[test]
    fn resume_cwd_for_slot_returns_project_root_for_non_git_worker() {
        // Non-git project: worker_tag_dir leaves the path as the
        // project root, so the fallback returns the root verbatim.
        let (ws, _rx) = Workspace::testing_stub();
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "non-git-proj",
            "/tmp/test-non-git",
            "implementer",
            "worker-uuid-non-git",
            false,
        );
        let resolved = ws.resume_cwd_for_slot(&session_key);
        assert_eq!(
            resolved,
            project_root.to_string_lossy(),
            "non-git worker resume cwd must equal the project root (no worktree subdir)",
        );
    }

    #[test]
    fn resume_cwd_for_slot_returns_empty_for_unknown_session() {
        // Non-worker, non-catalog session - the function returns
        // empty string and lets the bridge surface ConnectionFailed
        // (current behaviour for genuinely-orphan sessions).
        let (ws, _rx) = Workspace::testing_stub();
        let unknown = SessionSlot::from_str_for_test("not-a-known-session");
        assert_eq!(ws.resume_cwd_for_slot(&unknown), "");
    }

    #[test]
    fn cwd_for_session_resolves_a_git_worker_with_no_catalog_row() {
        // The catalog holds no worker rows at all (the boot scan hides
        // worker-tagged sessions; the Connected handler skips the
        // mirror for workers), so every worker resolves through the
        // registry - not just a resume.
        let (ws, _rx) = Workspace::testing_stub();
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "gateway-backend",
            "/tmp/test-gateway-cwd",
            "pyth-review-fixes",
            "worker-uuid-cwd",
            true,
        );
        assert_eq!(
            ws.cwd_for_session(&session_key).as_deref(),
            project_root.join(".claude/worktrees/pyth-review-fixes").to_str(),
        );
    }

    /// The project-root arm answers LEADS ONLY. `seed_project_and_worker`
    /// cannot show this: its slot names no loaded project, so
    /// `project_for_slot` misses and BOTH arms fall through to the
    /// registry's answer - which is how the shadowing regression passed
    /// every existing cwd test. The slot has to name a loaded project for
    /// the two arms to disagree, and the wrong one here sends a git
    /// worker's resume to the project root while its JSONL sits under
    /// the worktree (#245 Layer B).
    #[test]
    fn a_worker_slot_does_not_resolve_to_its_project_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        let view =
            ws.list_projects().into_iter().find(|v| v.name == "forge").expect("fixture project");
        let slot = SessionSlot::worker(&view.org, &view.name, "implementer");
        ws.insert_live_worker(&view.key, worker_entry("implementer", &slot, true));

        let resolved = ws.cwd_for_session(&slot);
        let expected = view.path.join(".claude/worktrees/implementer");
        assert_eq!(
            resolved.as_deref(),
            expected.to_str(),
            "the registry composes the worktree; the project root must not shadow it",
        );
    }

    /// The other arm of the same lookup, pinned next to it so the pair
    /// says what the split is: a lead's cwd IS its project root, read
    /// from the slot rather than from the catalog, because the catalog is
    /// keyed by the id the CLI adopted and `/new` replaces that.
    #[test]
    fn a_lead_slot_resolves_to_its_project_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        let view =
            ws.list_projects().into_iter().find(|v| v.name == "forge").expect("fixture project");

        let resolved = ws.cwd_for_session(&SessionSlot::lead(&view.org, &view.name));
        assert_eq!(resolved.as_deref(), view.path.to_str(), "a lead runs in its project root");
    }

    #[test]
    fn cwd_for_session_is_none_when_the_workers_project_is_not_loaded() {
        // The contradiction the WARN names: the registry knows the
        // worker's project_key and label, but no loaded project
        // matches that key, so no path can be composed. Unreachable
        // while forge.toml and `live_workers` agree.
        let (ws, _rx) = Workspace::testing_stub();
        let session_key = SessionSlot::from_str_for_test("worker-uuid-orphan");
        ws.insert_live_worker(
            &ProjectKey::new("stale-key".to_owned()),
            worker_entry("implementer", &session_key, true),
        );
        assert!(ws.cwd_for_session(&session_key).is_none());
    }

    /// The cwd read is not a catalog read any more: a slot that names
    /// no declared project resolves through the worker registry, and a
    /// catalog row for the same session carrying a different cwd is not
    /// consulted at all. Seeding BOTH with different paths is what makes
    /// the two rules distinguishable - under the old catalog-first
    /// precedence the row's path was the one that won.
    #[test]
    fn resume_cwd_for_slot_ignores_a_catalog_row_carrying_another_cwd() {
        let (ws, _rx) = Workspace::testing_stub();
        let session_id = "shared-session-uuid";
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "precedence-proj",
            "/tmp/test-precedence",
            "implementer",
            session_id,
            true,
        );
        // A catalog row for the same session, pointing somewhere else:
        // it is not a cwd source for this lookup.
        ws.record_connected_session("/tmp/test-precedence-catalog-cwd", session_id, None);
        let resolved = ws.resume_cwd_for_slot(&session_key);
        assert_eq!(
            resolved,
            project_root.join(".claude/worktrees/implementer").to_string_lossy(),
            "the worker registry decides the cwd; the catalog row is not read",
        );
    }

    /// Read/pick consistency: the cwd `resume_cwd_for_slot` hands
    /// `claude --resume` for a git worker encodes to the SAME storage
    /// key `build_resume_map_from_sessions` scopes candidates to (both
    /// go through `project_key_for_directory(worker_tag_dir(...))`), so
    /// a picked session always lives in the dir the resume read looks
    /// under - the pick and the read can't diverge the way the head-read
    /// cwd allowed.
    #[test]
    fn git_worker_resume_cwd_encodes_to_scoped_storage_key() {
        let (ws, _rx) = Workspace::testing_stub();
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "playground",
            "/tmp/test-playground-consistency",
            "gpt-tutor",
            "worker-uuid-consistency",
            true,
        );
        let read_key = forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            &ws.resume_cwd_for_slot(&session_key),
        ));
        let scoped_key = forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            crate::mcp::workers::types::worker_tag_dir(&project_root, "gpt-tutor", true)
                .to_string_lossy()
                .as_ref(),
        ));
        assert_eq!(
            read_key, scoped_key,
            "resume read dir and resume-map scope key must be the same storage folder",
        );
    }

    // ---------------------------------------------------------------
    // The selection walk and `session_chip_for`. Build a real
    // workspace from the local `make_workspace_dir_246` helper (single
    // account "Stargate", single project "forge") + manually drive the
    // loading state via account_pool().set_*().
    // ---------------------------------------------------------------

    fn make_workspace_dir_246() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// A project `model` fills the CLI's model slots: all five slot
    /// variables carry that one model on the spawned child.
    #[tokio::test]
    async fn a_project_model_fills_the_cli_model_slots() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace =
            std::sync::Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_gateway_ready(true);
        workspace.seed_test_ready_account("Stargate");
        let handle = workspace
            .get_agent_handle(
                SessionTarget::Default,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("spawn");
        let env = handle.env();
        for var in MODEL_SLOT_VARIABLES {
            assert_eq!(
                env.get(var).map(String::as_str),
                Some("claude-sonnet-5"),
                "{var} carries the project model",
            );
        }
    }

    /// A spawn whose target resolves to no configured project is
    /// refused, naming the session and both ways out. It used to spawn
    /// unregistered, carrying the account's real credential and a
    /// direct base URL - a session the gateway could not see, rotate or
    /// cool, and one whose dir forge.toml does not describe.
    #[tokio::test]
    async fn a_spawn_resolving_to_no_project_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_gateway_ready(true);
        workspace.seed_test_ready_account("Stargate");
        let target = SessionTarget::Session(SessionSlot::from_str_for_test("orphan-uuid"));
        let error = workspace
            .get_agent_handle(
                target,
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .err()
            .expect("a session mapping to no project must not spawn");

        let message = format!("{error}");
        assert!(
            message.contains("orphan-uuid"),
            "the refusal names the session, so the row is identifiable: {message}",
        );
        assert!(message.contains("Add that project"), "and names the way forward: {message}");
        assert!(
            workspace.pool.lock().is_empty(),
            "nothing is pooled, so no unregistered child is running",
        );
    }

    /// `project_would_bind` is the whole input to the launchpad's click
    /// gate and to its hint row, so an always-false regression would
    /// leave every project unclickable and nothing else would fail.
    #[tokio::test]
    async fn project_would_bind_tracks_the_walk() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let project = workspace.list_projects().into_iter().next().expect("one project");

        assert!(
            !workspace.project_would_bind(&project.key),
            "an account still resolving is nothing the walk can pick",
        );

        workspace.seed_test_ready_account("Stargate");
        assert!(
            workspace.project_would_bind(&project.key),
            "a ready account declaring the project's model is what a spawn lands on",
        );

        workspace.seed_test_account_state("Stargate", forge_gateway::LoadingState::Bailed);
        assert!(
            workspace.project_would_bind(&project.key),
            "a bailed account is still picked when nothing else declares the model, \
             so the row stays clickable",
        );

        // Agent-cooling, which is the one settled state the walk refuses:
        // it is what renders the launchpad's dim hint on a project that
        // does declare a model.
        workspace.seed_test_ready_account("Stargate");
        let an_hour_out = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("clock after the epoch")
            .as_secs()
            + 3600;
        workspace.gateway.report_probe_limit(&AccountKey("Stargate".to_owned()), Some(an_hour_out));
        assert!(
            !workspace.project_would_bind(&project.key),
            "every declaring account cooling is nothing to spawn on, so the row blocks",
        );
    }

    /// The spawn's notice: an account the walk had to take while
    /// saturated or bailed comes back named, one with room does not, so
    /// the tool result carries a notice exactly when there is something
    /// to warn about.
    #[tokio::test]
    async fn a_degraded_account_is_named_and_an_account_with_room_is_not() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let key = AccountKey("Stargate".to_owned());

        workspace.account_pool().set_usage(&key, usage_at(100.0));
        assert_eq!(
            workspace.degraded_account_name("Stargate").as_deref(),
            Some("Stargate"),
            "a saturated account is named, so the spawn warns the worker may throttle",
        );

        workspace.account_pool().set_usage(&key, usage_at(10.0));
        assert_eq!(
            workspace.degraded_account_name("Stargate"),
            None,
            "an account with room is not named, so the result carries no notice",
        );

        workspace.account_pool().set_loading(&key, forge_gateway::LoadingState::Bailed);
        assert_eq!(
            workspace.degraded_account_name("Stargate").as_deref(),
            Some("Stargate"),
            "a bailed account is named too",
        );
    }

    /// Two accounts and a project that declares no model, which is the
    /// fixture for `apply_project_model`'s absent-model case.
    fn make_workspace_dir_lead_and_worker() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Alpha", "Beta"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Alpha"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Beta"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// The probe-glue edge: record_usage_success with any window at
    /// the cap reports the gateway, which rotates every session bound
    /// to that account. A probe under the cap reports nothing.
    #[tokio::test]
    async fn record_usage_success_reports_probe_exhaustion_to_the_gateway() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let pool = workspace.account_pool();
        pool.set_usage(&AccountKey("Stargate".to_owned()), usage_at(10.0));
        workspace.gateway.bindings.bind("Org", "forge", "s1", AccountKey("Stargate".to_owned()));
        workspace.record_usage_success(&AccountKey("Stargate".to_owned()), usage_at(100.0));
        assert!(
            workspace.gateway.bindings.binding_for("Org", "forge", "s1").is_none(),
            "a probe verdict at the cap rotates the bound session",
        );

        workspace.gateway.bindings.bind("Org", "forge", "s2", AccountKey("Stargate".to_owned()));
        workspace.record_usage_success(&AccountKey("Stargate".to_owned()), usage_at(10.0));
        assert!(
            workspace.gateway.bindings.binding_for("Org", "forge", "s2").is_some(),
            "a probe under the cap rotates nothing",
        );
    }

    /// A write to the pool is announced.
    ///
    /// The home's account card is a snapshot field with no stream of its own:
    /// the per-account loading state, the settled flag and the listener's
    /// readiness all move without any seat's news, so a page that read them
    /// once drew `0 ready, probing` for the life of the connection while the
    /// pool had been ready for minutes. Nothing else in the stream says the
    /// pool moved, so the announcement is the only way a subscriber hears it.
    #[tokio::test]
    async fn a_probe_that_writes_the_pool_announces_it() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut updates = workspace.subscribe();
        // The boot backlog, so what the assertion reads is this call's news
        // rather than something emitted before it.
        while updates.try_recv().is_ok() {}

        workspace.record_usage_success(&AccountKey("Stargate".to_owned()), usage_at(10.0));

        let announced = std::iter::from_fn(|| updates.try_recv().ok())
            .any(|update| matches!(update, SessionUpdate::AccountsChanged));
        assert!(announced, "a probe that wrote the pool announced nothing");
    }

    /// An org whose primaries cannot serve the project's model and whose
    /// fallback can: the walk reaches the fallback because the primaries
    /// declare nothing it serves.
    fn make_workspace_dir_fallback_only_model() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Personal"]
fallback_accounts = ["OpenRouter-TM"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "deepseek-v4.1-flash"

[[accounts]]
display_name = "Personal"
token = "t"
models = ["claude-opus-5"]
provider = "anthropic"

[[accounts]]
display_name = "OpenRouter-TM"
token = "t"
models = ["deepseek-v4.1-flash"]
provider = "openrouter"
base_url = "https://openrouter.ai/api"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// Two primaries where only the second serves the project's model.
    fn make_workspace_dir_model_split() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Personal", "OpenRouter-TM"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "deepseek-v4.1-flash"

[[accounts]]
display_name = "Personal"
token = "t"
models = ["claude-opus-5"]
provider = "anthropic"

[[accounts]]
display_name = "OpenRouter-TM"
token = "t"
models = ["deepseek-v4.1-flash"]
provider = "openrouter"
base_url = "https://openrouter.ai/api"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// The project's model is served only by a fallback account: the walk
    /// reaches it, so the session lands on the declaring account rather
    /// than on a primary that cannot serve it.
    #[tokio::test]
    async fn a_model_declaring_project_spawns_on_the_fallback_that_serves_it() {
        let dir = make_workspace_dir_fallback_only_model();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let pool = workspace.account_pool();
        pool.set_usage(&AccountKey("Personal".to_owned()), usage_at(10.0));
        pool.set_usage(&AccountKey("OpenRouter-TM".to_owned()), usage_at(10.0));
        let handle = workspace
            .get_agent_handle(
                SessionTarget::Named("forge".to_owned()),
                SessionLaunchSettings::default(),
                &crate::protocol::SpawnRole::Lead,
            )
            .expect("the declaring fallback serves the spawn");
        assert_eq!(
            handle.display_name().as_deref(),
            Some("OpenRouter-TM"),
            "the spawn lands on the account that declares the project's model",
        );
    }

    /// The spawn pins the project's canonical model into the launch
    /// settings - which is how the session's model reaches the CLI - and
    /// a project that declares no model leaves the caller's pin alone.
    #[test]
    fn the_project_model_is_pinned_into_the_launch_settings() {
        let dir = make_workspace_dir_model_split();
        let config = crate::config::load_from_dir(dir.path()).expect("load");
        let project = config.projects.iter().find(|p| p.name == "forge").expect("forge project");
        let mut settings = SessionLaunchSettings {
            settings: Some(serde_json::json!({"effortLevel": "max", "model": "opus"})),
            ..SessionLaunchSettings::default()
        };
        apply_project_model(Some(project), &mut settings);
        let document = settings.settings.expect("document");
        assert_eq!(
            document["model"],
            serde_json::json!("deepseek-v4.1-flash"),
            "the canonical model replaces the caller's pin",
        );
        assert_eq!(
            document["effortLevel"],
            serde_json::json!("max"),
            "the other settings keys survive the pin",
        );

        let bare_dir = make_workspace_dir_lead_and_worker();
        let bare_config = crate::config::load_from_dir(bare_dir.path()).expect("load");
        let mut bare_settings = SessionLaunchSettings {
            settings: Some(serde_json::json!({"model": "opus"})),
            ..SessionLaunchSettings::default()
        };
        apply_project_model(bare_config.projects.first(), &mut bare_settings);
        assert_eq!(
            bare_settings.settings.expect("document")["model"],
            serde_json::json!("opus"),
            "a project that declares no model leaves the caller's pin alone",
        );
    }

    /// The refusal is a stated requirement: it names the project, the
    /// model, the org and the accounts considered.
    #[test]
    fn the_refusal_names_the_project_model_org_and_accounts_considered() {
        let error = WorkspaceError::NoAccountServesProjectModel {
            project: "steve".to_owned(),
            org: "Busytools".to_owned(),
            model: "glm-5.3-flash".to_owned(),
            accounts: "Zai, Personal".to_owned(),
        };
        let message = error.to_string();
        for needle in ["steve", "glm-5.3-flash", "Busytools", "Zai, Personal"] {
            assert!(message.contains(needle), "the refusal must name {needle}: {message}");
        }
    }

    fn usage_at(five_hour_util: f64) -> forge_primitives::usage::UsageSnapshot {
        forge_primitives::usage::UsageSnapshot {
            source: forge_primitives::usage::UsageSourceKind::Oauth,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
            five_hour: Some(forge_primitives::usage::UsageWindow {
                utilization: five_hour_util,
                resets_at: Some(
                    std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                ),
                reset_description: None,
            }),
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
            balance: None,
        }
    }

    #[tokio::test]
    async fn a_chip_needs_an_account_the_walk_can_find() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        assert!(
            workspace.session_chip_for(&project_key).is_none(),
            "nothing is Ready yet, so the walk finds no account to chip with",
        );
    }

    #[tokio::test]
    async fn session_chip_for_normal_branch_for_ready_account() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.account_pool().set_usage(
            &AccountKey("Stargate".to_owned()),
            forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                // 5h window not at cap -> Normal branch.
                five_hour: Some(forge_primitives::usage::UsageWindow {
                    utilization: 30.0,
                    resets_at: Some(
                        std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                    ),
                    reset_description: None,
                }),
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
                spend: None,
                balance: None,
            },
        );
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key).expect("chip");
        assert_eq!(chip.state, SessionChipState::Normal);
        assert_eq!(chip.account_name, "Stargate");
    }

    #[tokio::test]
    async fn session_chip_for_at_cap_branch() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.account_pool().set_usage(
            &AccountKey("Stargate".to_owned()),
            forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                // 5h window at 100% with future resets_at -> AtCap branch.
                five_hour: Some(forge_primitives::usage::UsageWindow {
                    utilization: 100.0,
                    resets_at: Some(
                        std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                    ),
                    reset_description: None,
                }),
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
                spend: None,
                balance: None,
            },
        );
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key).expect("chip");
        assert_eq!(chip.state, SessionChipState::AtCap);
    }

    #[tokio::test]
    async fn session_chip_for_at_cap_on_weekly_window() {
        // A weekly (7-day) cap alone, 5h window clear, must still flag
        // the chip AtCap - saturation is any-window, not 5h-only.
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.account_pool().set_usage(
            &AccountKey("Stargate".to_owned()),
            forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                five_hour: None,
                seven_day: Some(forge_primitives::usage::UsageWindow {
                    utilization: 100.0,
                    resets_at: Some(
                        std::time::SystemTime::now() + std::time::Duration::from_secs(86_400),
                    ),
                    reset_description: None,
                }),
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
                spend: None,
                balance: None,
            },
        );
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key).expect("chip");
        assert_eq!(chip.state, SessionChipState::AtCap);
    }

    #[tokio::test]
    async fn session_chip_for_bailed_branch() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        // Snapshot first, then bail: the walk reads the loading state,
        // so a chip needs the account first made pickable.
        workspace.account_pool().set_usage(
            &AccountKey("Stargate".to_owned()),
            forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                five_hour: None,
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
                spend: None,
                balance: None,
            },
        );
        // Now flip to Bailed on an auth failure - the red class.
        workspace
            .account_pool()
            .set_loading(&AccountKey("Stargate".to_owned()), forge_gateway::LoadingState::Bailed);
        workspace.account_pool().set_last_error(
            &AccountKey("Stargate".to_owned()),
            forge_gateway::UsageFetchStatus::Unauthorized,
            None,
        );
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key).expect("chip");
        assert_eq!(chip.state, SessionChipState::Bailed);
    }

    #[tokio::test]
    async fn session_chip_for_degraded_branch() {
        // A Bailed account with no auth class recorded - the
        // shape-drift settle, or a rate limit - is the warning-yellow
        // Degraded chip, the same split the account rows render.
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.account_pool().set_usage(
            &AccountKey("Stargate".to_owned()),
            forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                five_hour: None,
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
                spend: None,
                balance: None,
            },
        );
        workspace
            .account_pool()
            .set_loading(&AccountKey("Stargate".to_owned()), forge_gateway::LoadingState::Bailed);
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key).expect("chip");
        assert_eq!(chip.state, SessionChipState::Degraded);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod kick_dispatcher_tests {
    //! Cover for `start_kick_dispatcher` plus `enqueue_kick` (#259).
    //!
    //! Tests observe via `command_intercept` (`enable_test_dispatch_intercept`
    //! plus `drain_test_dispatch_buffer`); the drainer calls
    //! `Workspace::dispatch_forged_prompt` for each `KickRequest`, which the
    //! intercept buffer captures verbatim and whose prompt frame the update
    //! stream carries.
    //!
    //! Time is paused (`start_paused = true`) so the drainer's
    //! `tokio::time::sleep(KICK_DISPATCH_INTERVAL)` advances only when
    //! the test explicitly advances the clock. Without that, the
    //! drainer would race the assertions in real time.
    use super::*;
    use crate::protocol::Command;
    use std::time::Duration;

    /// A drained kick draws the words it sends the worker.
    ///
    /// They reach the model on stdin and the CLI does not echo them, so the
    /// frame is the only thing a view can draw them from - and reverting the
    /// drainer to the delivery path leaves every dispatch-side test in here
    /// green, which is why this one asserts the frame.
    #[tokio::test(start_paused = true)]
    async fn a_drained_kick_draws_its_words() {
        let (workspace, mut update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();

        let key = sk("draws");
        workspace
            .enqueue_kick(KickRequest { slot: key.clone(), prompt_body: "get on with it".into() });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let mut frame = None;
        while let Ok(update) = update_rx.try_recv() {
            if let crate::protocol::SessionUpdate::ChatAppended { key: addressed, msg, .. } = update
                && addressed == key
            {
                frame = Some(msg);
            }
        }
        let Some(Message::User { message, .. }) = frame else {
            panic!("a drained kick draws the words the worker received")
        };
        assert!(
            matches!(
                message.content.first(),
                Some(forge_primitives::ContentBlock::Text { text, .. }) if text == "get on with it"
            ),
            "the frame carries the kick's prose: {message:?}",
        );
    }

    /// Helper: a session key for kick tests.
    fn sk(name: &str) -> SessionSlot {
        SessionSlot::from_str_for_test(format!("kick-test-{name}"))
    }

    /// Helper: assert the intercept buffer's Prompt commands match
    /// `expected` SessionSlots in order. Filters out any non-Prompt
    /// commands the dispatch path might queue.
    fn assert_dispatched_kick_keys(
        workspace: &Arc<Workspace>,
        expected: &[SessionSlot],
        context: &str,
    ) {
        let dispatched = workspace.drain_test_dispatch_buffer();
        let keys: Vec<SessionSlot> = dispatched
            .into_iter()
            .filter_map(|c| match c {
                Command::Prompt { key, .. } | Command::PromptUnder { key, .. } => Some(key),
                _ => None,
            })
            .collect();
        assert_eq!(keys, expected.to_vec(), "{context}: dispatched keys mismatch");
    }

    /// First-kick latency is zero (drainer pulls immediately), and
    /// the SECOND kick waits `KICK_DISPATCH_INTERVAL` before firing.
    /// Pinning the stagger interval prevents a regression where the
    /// drainer sleeps before its first send (which would be a
    /// straightforward off-by-one bug given the loop's structure).
    #[tokio::test(start_paused = true)]
    async fn dispatcher_fires_first_kick_immediately_then_staggers() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();

        let a = sk("a");
        let b = sk("b");
        workspace.enqueue_kick(KickRequest { slot: a.clone(), prompt_body: "kick a".into() });
        workspace.enqueue_kick(KickRequest { slot: b.clone(), prompt_body: "kick b".into() });

        // Yield once so the drainer task gets a turn; it should fire
        // the first kick before sleeping. With paused time the sleep
        // doesn't advance, so the SECOND kick stays pending.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_dispatched_kick_keys(&workspace, std::slice::from_ref(&a), "after first yield");

        // Advance time past the interval; drainer wakes, fires the
        // second kick, then sleeps again with the queue now empty.
        tokio::time::advance(KICK_DISPATCH_INTERVAL).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_dispatched_kick_keys(&workspace, std::slice::from_ref(&b), "after interval advance");
    }

    /// Multi-worker burst: 7 simultaneous enqueues produce exactly
    /// one dispatch per interval, all 7 dispatched after 6 advances.
    /// Mirrors the issue's reproduction shape (forge boot with 5+
    /// team workers).
    #[tokio::test(start_paused = true)]
    async fn dispatcher_staggers_seven_simultaneous_kicks_one_per_interval() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();

        let keys: Vec<SessionSlot> = (0..7).map(|i| sk(&format!("worker-{i}"))).collect();
        for key in &keys {
            workspace.enqueue_kick(KickRequest {
                slot: key.clone(),
                prompt_body: format!("kick {}", key.display()),
            });
        }

        // First kick fires before any sleep.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_dispatched_kick_keys(&workspace, &keys[0..1], "boot tick");

        // Six more intervals → six more kicks → all 7 dispatched.
        for i in 1..7 {
            tokio::time::advance(KICK_DISPATCH_INTERVAL).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            assert_dispatched_kick_keys(
                &workspace,
                &keys[i..=i],
                &format!("after interval advance {i}"),
            );
        }
    }

    /// `start_kick_dispatcher` is idempotent: a second call after the
    /// receiver has been taken finds the slot empty and no-ops. A
    /// regression that spawns two drainers would fire each kick TWICE
    /// (both drainers would race for the same receiver - actually
    /// only the first would get the item due to mpsc semantics, but
    /// the SECOND drainer would burn task slots forever waiting on
    /// an empty channel). Pin by counting dispatches against a known
    /// queue.
    #[tokio::test(start_paused = true)]
    async fn start_kick_dispatcher_is_idempotent() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.start_kick_dispatcher();
        workspace.start_kick_dispatcher(); // no-op second call

        workspace.enqueue_kick(KickRequest { slot: sk("only"), prompt_body: "k".into() });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let dispatched = workspace.drain_test_dispatch_buffer();
        let prompts: Vec<&Command> = dispatched
            .iter()
            .filter(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. }))
            .collect();
        assert_eq!(prompts.len(), 1, "second start_kick_dispatcher must not duplicate dispatches");
    }

    /// `enqueue_kick` after workspace drop is logged but doesn't
    /// panic. The channel's sender is held on `Workspace`, so dropping
    /// the workspace closes the channel; the drainer (if still alive)
    /// exits its recv loop. Verifying the no-panic shape protects the
    /// shutdown-race window where a final Connected event might queue
    /// a kick after the drop began.
    ///
    /// We don't drop the Arc here (testing_stub gives one out and the
    /// test holds it for the duration). What we DO verify: `enqueue_kick`
    /// returns successfully when the dispatcher hasn't been started -
    /// the message just sits in the channel until either the drainer
    /// is started or the workspace is dropped. Either way, no panic.
    #[tokio::test]
    async fn enqueue_kick_without_dispatcher_started_does_not_panic() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        // Note: NOT calling start_kick_dispatcher.
        workspace.enqueue_kick(KickRequest { slot: sk("orphan"), prompt_body: "k".into() });
        // No assertion target other than "we got here without panicking".
        // A future change that makes enqueue_kick require a started
        // dispatcher would fail this test.
        let _ = Duration::from_millis(0); // touch Duration to keep the use site live
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod catalog_scan_tests {
    use super::*;
    use crate::protocol::Command;
    use forge_agent::client::SessionLaunchSettings;
    use std::fs;
    use tempfile::tempdir;

    const LEAD_UUID: &str = "00000000-0000-4000-8000-000000000001";
    const WORKER_UUID: &str = "00000000-0000-4000-8000-000000000002";

    /// One project rooted inside the tempdir plus a transcript whose
    /// head carries that cwd, so the scan groups the session under the
    /// project's key the same way boot did.
    fn scan_fixture_dir() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        let project_path = dir.path().join("proj");
        let toml = format!(
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]
[[orgs.projects]]
name = "proj"
path = "{}"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
            project_path.display()
        );
        fs::write(forge_toml_path(dir.path()), toml).expect("write forge.toml");
        write_session_fixture(dir.path(), &project_path.display().to_string(), LEAD_UUID, None);
        dir
    }

    fn write_session_fixture(
        config_dir: &std::path::Path,
        project_path: &str,
        session_id: &str,
        tag: Option<&str>,
    ) {
        let key =
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some(project_path));
        let project_dir = config_dir.join("projects").join(key);
        fs::create_dir_all(&project_dir).expect("project dir");
        let mut body = format!(
            "{{\"type\":\"user\",\"timestamp\":\"2026-09-05T00:00:00.000Z\",\"cwd\":\"{project_path}\",\"message\":{{\"content\":\"opening prompt\"}}}}\n"
        );
        if let Some(tag) = tag {
            body = format!(
                "{body}{{\"type\":\"tag\",\"tag\":\"{tag}\",\"sessionId\":\"{session_id}\"}}\n"
            );
        }
        fs::write(project_dir.join(format!("{session_id}.jsonl")), body).expect("write jsonl");
    }

    /// A spawn does not park on the background catalog scan. It reaches
    /// the pool immediately against an unloaded catalog, keyed by an id
    /// forge minted rather than the lead transcript that catalog holds -
    /// the store is the resume source now, and it is empty here.
    #[tokio::test]
    async fn a_spawn_before_the_scan_does_not_wait_for_it() {
        let dir = scan_fixture_dir();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_gateway_ready(true);
        workspace.seed_test_ready_account("Stargate");
        assert!(!workspace.catalog_ready(), "new_for_test leaves the scan unstarted");

        workspace
            .dispatch(Command::StartDefault {
                project_name: Some("proj".to_owned()),
                launch_settings: SessionLaunchSettings::default(),
            })
            .expect("dispatch");

        let keys: Vec<String> =
            workspace.pool.lock().keys().map(forge_primitives::SessionSlot::display).collect();
        assert_eq!(keys.len(), 1, "the spawn reached the pool without waiting for the scan");
        assert_ne!(
            keys[0], LEAD_UUID,
            "and keys to an id forge minted, not the catalog's lead transcript",
        );
    }

    /// The background scan swaps the catalog in and announces it: the
    /// update arrives, readiness flips, and the default catalog hides
    /// worker-tagged sessions exactly as the synchronous scan did.
    #[tokio::test]
    async fn catalog_scan_reports_loaded_and_fills_project_views() {
        let dir = scan_fixture_dir();
        write_session_fixture(
            dir.path(),
            &dir.path().join("proj").display().to_string(),
            WORKER_UUID,
            Some("forge:worker:implementer"),
        );
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut update_rx = workspace.subscribe();
        assert!(workspace.list_projects()[0].sessions.is_empty(), "catalog starts empty");

        workspace.start_catalog_scan();

        let update = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
            .await
            .expect("scan finishes")
            .expect("channel open");
        assert!(
            matches!(update, SessionUpdate::CatalogLoaded),
            "expected CatalogLoaded, got {update:?}"
        );
        assert!(workspace.catalog_ready());

        let projects = workspace.list_projects();
        assert_eq!(projects.len(), 1);
        assert_eq!(
            projects[0].sessions.len(),
            1,
            "worker-tagged sessions stay hidden from the default catalog"
        );
        assert_eq!(projects[0].sessions[0].session.as_str(), LEAD_UUID);
    }

    /// No spawn waits on the boot catalog scan any more: the lead's
    /// resume target comes from the store, so a spawn dispatched against
    /// an unloaded catalog lands immediately, keyed by the project's
    /// lead slot rather than by the lead transcript the catalog holds.
    #[tokio::test]
    async fn a_spawn_does_not_wait_for_the_catalog_scan() {
        let dir = scan_fixture_dir();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        workspace.seed_test_ready_account("Stargate");
        assert!(!workspace.catalog_ready(), "the scan has not landed yet");

        workspace
            .dispatch(Command::StartDefault {
                project_name: Some("proj".to_owned()),
                launch_settings: SessionLaunchSettings::default(),
            })
            .expect("dispatch");

        let keys: Vec<String> =
            workspace.pool.lock().keys().map(forge_primitives::SessionSlot::display).collect();
        assert_eq!(keys.len(), 1, "the spawn ran immediately, unparked");
        assert_eq!(
            keys[0], "Default/proj/lead",
            "and keyed by the project's lead slot: the catalog's lead transcript is not a \
             routing key, and the store this spawn reads is empty: {keys:?}",
        );
    }

    /// The boot scan persists a tag-cache row per transcript even when
    /// the config dir is reached through a symlink, and the same pass
    /// prunes a seeded row whose transcript is gone - the prune is part
    /// of the scan, not only of the store fn.
    #[tokio::test]
    async fn boot_scan_warms_the_cache_and_prunes_stale_rows() {
        let real = scan_fixture_dir();
        write_session_fixture(
            real.path(),
            &real.path().join("proj").display().to_string(),
            WORKER_UUID,
            Some("forge:worker:implementer"),
        );
        let link_dir = tempfile::tempdir().expect("tempdir");
        let link = link_dir.path().join("cfg");
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink config dir");

        let workspace = Arc::new(Workspace::new_for_test(link).expect("new"));
        {
            let db = workspace.db.lock();
            crate::store::session_tags::store_all(
                db.as_ref().expect("test db present"),
                &[(
                    "/gone/deleted.jsonl".to_owned(),
                    forge_agent::userdata::catalog::scan::SessionTagScan {
                        tag: None,
                        scanned_len: 10,
                    },
                )],
            )
            .expect("seed stale row");
        }

        workspace.start_catalog_scan();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !workspace.catalog_ready() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scan finishes");

        let prior = {
            let db = workspace.db.lock();
            crate::store::session_tags::load_all(db.as_ref().expect("test db present"))
                .expect("load persisted cache")
        };
        assert!(!prior.is_empty(), "the boot scan persisted cache rows");

        let after = {
            let db = workspace.db.lock();
            crate::store::session_tags::load_all(db.as_ref().expect("test db present"))
                .expect("reload cache")
        };
        assert!(!after.contains_key("/gone/deleted.jsonl"), "the boot scan pruned the stale row");
    }

    /// The boot scan reaches a tree through an outbound `projects`
    /// symlink when that tree lives elsewhere.
    #[tokio::test]
    async fn boot_scan_reaches_an_outbound_projects_symlink() {
        let cfg = tempdir().expect("cfg tempdir");
        let tree = tempdir().expect("tree tempdir");
        let project_path = cfg.path().join("proj");
        fs::create_dir_all(&project_path).expect("project dir");
        let toml = format!(
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]
[[orgs.projects]]
name = "proj"
path = "{}"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
            project_path.display()
        );
        fs::write(forge_toml_path(cfg.path()), toml).expect("write forge.toml");
        write_session_fixture(
            tree.path(),
            &project_path.display().to_string(),
            WORKER_UUID,
            Some("forge:worker:implementer"),
        );
        std::os::unix::fs::symlink(tree.path().join("projects"), cfg.path().join("projects"))
            .expect("outbound projects symlink");

        let workspace = Arc::new(Workspace::new_for_test(cfg.path().to_owned()).expect("new"));
        workspace.start_catalog_scan();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !workspace.catalog_ready() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scan finishes");

        let prior = {
            let db = workspace.db.lock();
            crate::store::session_tags::load_all(db.as_ref().expect("test db present"))
                .expect("load persisted cache")
        };
        assert!(
            prior.keys().any(|key| key.contains("implementer") || !key.is_empty()),
            "the boot scan reached the external tree and persisted cache rows: {prior:?}",
        );
    }

    /// An outbound symlink whose target is not named `projects` still
    /// scans: the walk descends into `<root>/projects`, so a root
    /// derived from the target's parent would miss the tree entirely
    /// and the catalog would sit empty. The derivation falls back to
    /// the config dir, whose `projects` link resolves to the tree.
    #[tokio::test]
    async fn boot_scan_reaches_an_outbound_tree_not_named_projects() {
        let cfg = tempdir().expect("cfg tempdir");
        let tree = tempdir().expect("tree tempdir");
        let project_path = cfg.path().join("proj");
        fs::create_dir_all(&project_path).expect("project dir");
        let toml = format!(
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]
[[orgs.projects]]
name = "proj"
path = "{}"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
            project_path.display()
        );
        fs::write(forge_toml_path(cfg.path()), toml).expect("write forge.toml");
        write_session_fixture(tree.path(), &project_path.display().to_string(), LEAD_UUID, None);
        // The fixture writes a literal `projects` dir; the layout under
        // test is one that is NOT named that.
        fs::rename(tree.path().join("projects"), tree.path().join("sessions"))
            .expect("rename the tree to an odd name");
        std::os::unix::fs::symlink(tree.path().join("sessions"), cfg.path().join("projects"))
            .expect("outbound projects symlink to an oddly named dir");

        let workspace = Arc::new(Workspace::new_for_test(cfg.path().to_owned()).expect("new"));
        workspace.start_catalog_scan();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !workspace.catalog_ready() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scan finishes");

        let sessions = &workspace.list_projects()[0].sessions;
        assert_eq!(
            sessions.first().map(|s| s.session.as_str()),
            Some(LEAD_UUID),
            "the session behind the oddly named link is in the catalog"
        );
    }

    /// A session recorded live before the scan lands survives the
    /// scan's catalog swap: its transcript may not exist on disk yet,
    /// so the disk-built map absorbs the recorded rows rather than
    /// replacing them.
    #[tokio::test]
    async fn scan_swap_preserves_live_recorded_sessions() {
        let dir = scan_fixture_dir();
        let project_dir = dir.path().join("proj");
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));

        workspace.record_connected_session(&project_dir.display().to_string(), "live-uuid", None);
        workspace.start_catalog_scan();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !workspace.catalog_ready() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scan finishes");

        let sessions = &workspace.list_projects()[0].sessions;
        assert!(
            sessions.iter().any(|s| s.session.as_str() == "live-uuid"),
            "the live-recorded session survives the scan swap"
        );
        assert!(
            sessions.iter().any(|s| s.session.as_str() == LEAD_UUID),
            "the on-disk session is present alongside it"
        );
    }

    /// Live-recorded sessions keep their recorded (newest-first) order
    /// at the head of the merged catalog. The Projects pane reads
    /// `sessions.first()` for the row's relative time, so a reversed
    /// head shows the oldest connection's recency.
    #[tokio::test]
    async fn scan_swap_keeps_recorded_sessions_newest_first() {
        let dir = scan_fixture_dir();
        let project_dir = dir.path().join("proj");
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));

        workspace.record_connected_session(&project_dir.display().to_string(), "live-older", None);
        workspace.record_connected_session(&project_dir.display().to_string(), "live-newer", None);
        workspace.start_catalog_scan();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !workspace.catalog_ready() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scan finishes");

        let sessions = &workspace.list_projects()[0].sessions;
        assert_eq!(
            sessions[0].session.as_str(),
            "live-newer",
            "the latest connection leads the project"
        );
        assert_eq!(sessions[1].session.as_str(), "live-older");
        assert_eq!(sessions[2].session.as_str(), LEAD_UUID);
    }

    /// With no tokio runtime there is no scan to await, so readiness
    /// publishes immediately over an empty catalog and spawns fall
    /// through to fresh instead of parking forever.
    #[test]
    fn start_catalog_scan_without_a_runtime_publishes_readiness() {
        let dir = scan_fixture_dir();
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("new");

        workspace.start_catalog_scan();

        assert!(workspace.catalog_ready(), "no runtime: readiness publishes immediately");
        assert!(
            workspace.list_projects()[0].sessions.is_empty(),
            "no runtime: no scan ran, the catalog stays empty"
        );
    }
}

#[cfg(test)]
mod prompt_frame_origin_tests {
    //! A prompt's frame says where its words came from, and the answer is the
    //! entry the caller took rather than a value off the wire. One view draws
    //! on that answer - the terminal's own words are drawn at submit, another
    //! view's are not - so a wrong origin is a reader shown their own line
    //! twice, or a send nobody draws at all.
    use super::*;

    fn a_prompt(key: &SessionSlot) -> Command {
        Command::Prompt { key: key.clone(), text: "hello".to_owned(), attachments: Vec::new() }
    }

    /// A fleet with one seat, and this seat's stream off the core's fan-out.
    ///
    /// The prompt is ACCEPTED through the dispatch intercept, which is this
    /// crate's own way of saying "the core took it" without standing up a real
    /// subprocess - the stub alone leaves the seat with no command sender, so
    /// every dispatch is refused for a reason the fixture invented.
    fn a_fleet() -> (Arc<Workspace>, mpsc::UnboundedReceiver<SessionUpdate>, SessionSlot) {
        let (ws, rx) = Workspace::testing_stub();
        ws.seed_test_project("proj", "/tmp/prompt-frame-origin");
        let seat = SessionSlot::lead("TestOrg", "proj");
        ws.enable_test_dispatch_intercept();
        (ws, rx, seat)
    }

    /// The terminal's own entry, which is what `App::dispatch_command` takes.
    #[test]
    fn the_uis_own_entry_marks_the_frame_as_the_uis() {
        let (ws, mut rx, seat) = a_fleet();

        let dispatched = ws.dispatch(a_prompt(&seat));

        assert!(dispatched.is_ok(), "precondition: the core took the prompt: {dispatched:?}");
        assert!(
            matches!(
                rx.try_recv(),
                Ok(SessionUpdate::ChatAppended { origin: Some(PromptOrigin::Ui), key, .. })
                    if key == seat
            ),
            "the words the terminal drew at submit are marked as its own, or it draws them twice",
        );
    }

    /// The frame a typed dispatch forges carries the SAME id the wired prompt
    /// runs under.
    ///
    /// The lifecycle frames name only the id, so a frame minted apart from the
    /// wire would hold a row nothing can ever settle - the pile dead for that
    /// prompt in every view - and a page's copy of the message, which the CLI
    /// writes under the same id, would arrive as a stranger rather than as the
    /// row already held.
    #[test]
    fn a_prompt_frames_uuid_is_the_uuid_it_runs_under() {
        let (ws, mut rx, seat) = a_fleet();

        let dispatched = ws.dispatch_from_view(a_prompt(&seat));
        assert!(dispatched.is_ok(), "precondition: the core took the prompt: {dispatched:?}");

        let wired = ws.drain_test_dispatch_buffer();
        let wired_id = match wired.as_slice() {
            [Command::PromptUnder { uuid, .. }] => uuid.clone(),
            other => panic!("a typed dispatch wires the prompt under a minted id: {other:?}"),
        };
        assert!(!wired_id.is_empty(), "and a real one, not a blank");

        let mut frame_id: Option<String> = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::ChatAppended {
                msg: forge_primitives::Message::User { uuid, .. },
                ..
            } = update
            {
                frame_id = uuid;
            }
        }
        assert_eq!(
            frame_id.as_deref(),
            Some(wired_id.as_str()),
            "the forged frame and the wired prompt are one thing by id",
        );
    }

    /// `/new` from any composer is forge's own command, never the CLI's. The
    /// CLI answers that name as its own `/clear`, which rotates a
    /// conversation forge never records - no id minted, no `SessionReplaced` -
    /// so the seat is restarted here instead, and the words never reach it.
    #[test]
    fn a_new_prompt_restarts_the_seat_rather_than_reaching_the_cli() {
        let (ws, _rx, seat) = a_fleet();

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/new".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "the core takes a /new prompt: {dispatched:?}");
        let commands = ws.drain_test_dispatch_buffer();
        assert!(
            commands.iter().any(|c| matches!(c, Command::NewSession { key, .. } if key == &seat)),
            "the seat is restarted: {commands:?}",
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. })),
            "and the words are never forwarded to the CLI: {commands:?}",
        );
    }

    /// `/resume <id>` is forge's too, and for a harder reason than `/new`:
    /// the CLI classifies its own `/resume` as local-jsx, so it cannot run
    /// away from a terminal at all. The seat is re-spawned onto the named
    /// session, from the same settings a `/new` would carry.
    #[test]
    fn a_resume_prompt_resumes_the_seat_rather_than_reaching_the_cli() {
        let (ws, _rx, seat) = a_fleet();

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/resume 7f3a92e0".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "the core takes a /resume prompt: {dispatched:?}");
        let commands = ws.drain_test_dispatch_buffer();
        assert!(
            commands.iter().any(|c| matches!(
                c,
                Command::ResumeSession { key, session_id, .. }
                    if key == &seat && session_id == "7f3a92e0"
            )),
            "the named session is resumed: {commands:?}",
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, Command::Prompt { .. } | Command::PromptUnder { .. })),
            "and the words are never forwarded to the CLI: {commands:?}",
        );
    }

    /// `/mode <id>` and `/model <id>` are the same commands the terminal
    /// dispatched, so a view neither of them can run still changes both.
    #[test]
    fn mode_and_model_prompts_dispatch_their_own_commands() {
        let (ws, _rx, seat) = a_fleet();

        for (text, expected) in [
            (
                "/mode plan",
                Box::new(|command: &Command| {
                    matches!(
                        command,
                        Command::SetMode { mode, .. }
                            if *mode == forge_primitives::permission::PermissionMode::Plan
                    )
                }) as Box<dyn Fn(&Command) -> bool>,
            ),
            (
                "/model sonnet",
                Box::new(
                    |command: &Command| matches!(command, Command::SetModel { model, .. } if model == "sonnet"),
                ),
            ),
        ] {
            let dispatched = ws.dispatch_from_view(Command::Prompt {
                key: seat.clone(),
                text: text.to_owned(),
                attachments: Vec::new(),
            });
            assert!(dispatched.is_ok(), "{text}: {dispatched:?}");
            let commands = ws.drain_test_dispatch_buffer();
            assert!(
                matches!(commands.as_slice(), [command] if expected(command)),
                "{text} dispatches its own command: {commands:?}",
            );
        }
    }

    /// A mode the CLI has no name for is answered rather than sent: the
    /// command carries the enum, so an unparsed one cannot be dispatched.
    #[test]
    fn a_mode_the_cli_does_not_have_is_answered() {
        let (ws, mut rx, seat) = a_fleet();

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/mode sideways".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "{dispatched:?}");
        // The echo of the reader's own words is one of these; the answer is
        // what this case is about (its ORDER is the case above, which does not
        // need the words' text).
        let notices: Vec<(NoticeSeverity, String)> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|update| match update {
                SessionUpdate::Notice { severity, text, .. } => Some((severity, text)),
                _ => None,
            })
            .collect();
        assert!(
            matches!(
                notices.as_slice(),
                [(NoticeSeverity::Error, text)] if text == "Unknown mode: sideways"
            ),
            "the reader is told the mode is not one the CLI has: {notices:?}",
        );
        assert!(ws.drain_test_dispatch_buffer().is_empty(), "and nothing is dispatched");
    }

    /// A forge command's answer draws under the words that asked for it.
    ///
    /// The echo and the answer are two updates on one stream, and a client
    /// draws them in arrival order: an answer emitted where the command was
    /// handled arrived before the reader's own words, so every socket client
    /// drew "Unknown mode: sideways" ABOVE the prompt it answers.
    #[test]
    fn a_forge_commands_answer_follows_the_words_it_answers() {
        let (ws, mut rx, seat) = a_fleet();

        ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/mode sideways".to_owned(),
            attachments: Vec::new(),
        })
        .expect("the core takes it");

        let first = rx.try_recv().expect("the echo is the first update on the stream");
        assert!(
            matches!(&first, SessionUpdate::ChatAppended { key, .. } if key == &seat),
            "the reader's own words are echoed before the answer: {first:?}",
        );
        let second = rx.try_recv().expect("the answer follows the echo");
        assert!(
            matches!(
                &second,
                SessionUpdate::Notice { severity: NoticeSeverity::Error, text, .. }
                    if text == "Unknown mode: sideways"
            ),
            "and the answer lands under them, where a client draws it: {second:?}",
        );
    }

    /// `/effort <level>` writes the document the next launch reads, rather
    /// than dispatching: the CLI carries no control request for effort.
    #[test]
    fn effort_writes_the_launch_document_and_answers() {
        let (ws, mut rx, seat) = a_fleet();
        // The pool's stub handle is bound to a scratch config dir, so this
        // write is a real one against a path no user reads.
        ws.seed_test_bound_session(&seat, "acct-a");
        let config_dir = ws.config_dir_for(&seat).expect("the pooled stub names a config dir");
        let path = config_dir.join("settings.json");
        let before = std::fs::read_to_string(&path).ok();

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/effort high".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "{dispatched:?}");
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("the effort level lands in the document"),
        )
        .expect("the document parses");
        assert_eq!(written["effortLevel"], serde_json::json!("high"));
        let notices: Vec<(NoticeSeverity, String)> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|update| match update {
                SessionUpdate::Notice { severity, text, .. } => Some((severity, text)),
                _ => None,
            })
            .collect();
        assert!(
            matches!(
                notices.as_slice(),
                [(NoticeSeverity::Info, text)] if text.contains("Effort: High")
            ),
            "and the reader is told it took: {notices:?}",
        );
        assert!(ws.drain_test_dispatch_buffer().is_empty(), "nothing is dispatched");

        match before {
            Some(contents) => {
                std::fs::write(&path, contents).expect("restore the stub's document");
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// A forge name invoked wrongly is answered with the command's own usage
    /// line, and dispatched nowhere: a mistyped command that reached the model
    /// would read as a question.
    #[test]
    fn a_wrong_forge_invocation_is_answered_rather_than_dispatched() {
        let (ws, mut rx, seat) = a_fleet();

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/resume".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "the core takes it: {dispatched:?}");
        let notices: Vec<(SessionSlot, NoticeSeverity, String)> =
            std::iter::from_fn(|| rx.try_recv().ok())
                .filter_map(|update| match update {
                    SessionUpdate::Notice { key, severity, text } => Some((key, severity, text)),
                    _ => None,
                })
                .collect();
        assert!(
            matches!(
                notices.as_slice(),
                [(key, NoticeSeverity::Error, text)]
                    if key == &seat && text == "Usage: /resume <session_id>"
            ),
            "the reader is told what the command wanted: {notices:?}",
        );
        let commands = ws.drain_test_dispatch_buffer();
        assert!(commands.is_empty(), "and nothing is dispatched: {commands:?}");
    }

    /// Slash text this session cannot run is answered where it is typed,
    /// rather than handed to the model as a question. What the session can
    /// run is what the CLI advertised, so the guard reads that and nothing
    /// else.
    #[test]
    fn a_slash_name_the_session_does_not_have_is_refused() {
        let (ws, mut rx, seat) = a_fleet();
        ws.seed_test_advertised_catalogues(
            &seat,
            vec![forge_primitives::AvailableCommand::new("compact", "Compact")],
            Vec::new(),
        );

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/spinner now".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "the core takes it: {dispatched:?}");
        // The refusal is answered after the echo of the words, so the two are
        // read off the stream together (the order is the case above).
        let notices: Vec<(NoticeSeverity, String)> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|update| match update {
                SessionUpdate::Notice { severity, text, .. } => Some((severity, text)),
                _ => None,
            })
            .collect();
        assert!(
            matches!(
                notices.as_slice(),
                [(NoticeSeverity::Error, text)] if text == "/spinner is not yet supported"
            ),
            "the reader is told the name is not one this session has: {notices:?}",
        );
        let commands = ws.drain_test_dispatch_buffer();
        assert!(commands.is_empty(), "and nothing reaches the model: {commands:?}");
    }

    /// The guard's clauses, one row each: prose with no slash is the model's,
    /// a name the CLI resolves is the CLI's, an advertised name is the CLI's,
    /// and only a name nothing has is refused.
    ///
    /// **A slash-led first word is refused whether or not words follow it**,
    /// which is the terminal's own rule. The two readings cannot be told
    /// apart at the first token, and the permissive one reopens the hole the
    /// guard exists for: `/spinner now`, a typo with an argument, would reach
    /// the model as a question.
    #[test]
    fn the_guard_refuses_only_a_name_nothing_has() {
        let (ws, mut rx, seat) = a_fleet();
        ws.seed_test_advertised_catalogues(
            &seat,
            vec![forge_primitives::AvailableCommand::new("compact", "Compact")],
            Vec::new(),
        );

        for (text, refusal) in [
            ("hello", None),
            ("/tmp is full, why?", Some("/tmp is not yet supported")),
            ("/help", None),
            ("/compact 3", None),
            ("/compact3", Some("/compact3 is not yet supported")),
        ] {
            // The rows share one fleet, so each starts from an empty stream:
            // the frame the row above drew would otherwise answer for this one.
            while rx.try_recv().is_ok() {}
            let dispatched = ws.dispatch_from_view(Command::Prompt {
                key: seat.clone(),
                text: text.to_owned(),
                attachments: Vec::new(),
            });
            assert!(dispatched.is_ok(), "{text}: {dispatched:?}");
            if let Some(expected) = refusal {
                // The echo of the words is on the stream too, after the fix
                // that put the answer under them; the refusal is what this row
                // is about.
                let notices: Vec<(NoticeSeverity, String)> =
                    std::iter::from_fn(|| rx.try_recv().ok())
                        .filter_map(|update| match update {
                            SessionUpdate::Notice { severity, text, .. } => Some((severity, text)),
                            _ => None,
                        })
                        .collect();
                assert!(
                    matches!(
                        notices.as_slice(),
                        [(NoticeSeverity::Error, text)] if text == expected
                    ),
                    "{text} is refused with {expected:?}, got {notices:?}",
                );
                assert!(ws.drain_test_dispatch_buffer().is_empty(), "{text} reaches nothing");
            } else {
                let commands = ws.drain_test_dispatch_buffer();
                assert!(
                    matches!(
                        commands.as_slice(),
                        [Command::Prompt { .. } | Command::PromptUnder { .. }]
                    ),
                    "{text} is the CLI's: {commands:?}",
                );
            }
        }
    }

    /// Effort is user-level, so it lands even when the seat has no session
    /// behind it: what it writes is the document a launch reads, not anything
    /// about a run.
    #[test]
    fn effort_lands_for_a_seat_with_no_live_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/prompt-frame-origin");
        ws.enable_test_dispatch_intercept();

        ws.dispatch_from_view(Command::Prompt {
            key: SessionSlot::lead("TestOrg", "proj"),
            text: "/effort low".to_owned(),
            attachments: Vec::new(),
        })
        .expect("the core takes it");

        let written = std::fs::read_to_string(dir.path().join("settings.json"))
            .expect("the document is written");
        let document: serde_json::Value = serde_json::from_str(&written).expect("it parses");
        assert_eq!(document["effortLevel"], serde_json::json!("low"));
        let notices: Vec<(NoticeSeverity, String)> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|update| match update {
                SessionUpdate::Notice { severity, text, .. } => Some((severity, text)),
                _ => None,
            })
            .collect();
        assert!(
            matches!(notices.as_slice(), [(NoticeSeverity::Info, _)]),
            "and the reader is told it took: {notices:?}",
        );
    }

    /// A spawn a view could not complete is completed by the core: a client
    /// has no config to read, so what it sends carries none, and the settings
    /// the launch reads are built from the same documents the terminal reads.
    #[test]
    fn a_spawn_without_settings_gets_them_from_the_documents() {
        // A config dir of its own, so the line the spawn carries is read from
        // a document this test wrote rather than from the machine's.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("settings.json"), br#"{ "effortLevel": "low" }"#)
            .expect("seed the document");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.enable_test_dispatch_intercept();

        ws.dispatch_from_view(Command::SpawnProject {
            project_name: "proj".to_owned(),
            launch_settings: SessionLaunchSettings::default(),
        })
        .expect("the core takes it");

        let commands = ws.drain_test_dispatch_buffer();
        let [Command::SpawnProject { launch_settings, .. }] = commands.as_slice() else {
            panic!("the spawn was not the command routed: {commands:?}");
        };
        let settings = launch_settings.settings.as_ref().expect("the documents filled it in");
        assert_eq!(
            settings["effortLevel"],
            serde_json::json!("low"),
            "the value the document holds, which a bare spawn would not have carried",
        );
    }

    /// The mirror: a spawn that built its own settings keeps them, so the
    /// terminal's snapshot is not silently replaced by the core's read.
    #[test]
    fn a_spawn_that_supplied_settings_keeps_them() {
        let (ws, _rx, _seat) = a_fleet();
        let supplied = SessionLaunchSettings {
            settings: Some(serde_json::json!({ "model": "haiku" })),
            ..SessionLaunchSettings::default()
        };

        ws.dispatch_from_view(Command::SpawnProject {
            project_name: "proj".to_owned(),
            launch_settings: supplied,
        })
        .expect("the core takes it");

        let commands = ws.drain_test_dispatch_buffer();
        let [Command::SpawnProject { launch_settings, .. }] = commands.as_slice() else {
            panic!("the spawn was not the command routed: {commands:?}");
        };
        assert_eq!(
            launch_settings.settings,
            Some(serde_json::json!({ "model": "haiku" })),
            "the caller's own settings are left alone",
        );
    }

    /// A name the CLI advertised is the CLI's, and goes to it.
    #[test]
    fn a_slash_name_the_cli_advertised_is_forwarded() {
        let (ws, _rx, seat) = a_fleet();
        ws.seed_test_advertised_catalogues(
            &seat,
            vec![forge_primitives::AvailableCommand::new("compact", "Compact")],
            Vec::new(),
        );

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/compact".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "the core takes it: {dispatched:?}");
        let commands = ws.drain_test_dispatch_buffer();
        assert!(
            matches!(
                commands.as_slice(),
                [Command::Prompt { text, .. } | Command::PromptUnder { text, .. }]
                    if text == "/compact"
            ),
            "an advertised name is the CLI's: {commands:?}",
        );
    }

    /// A session that has advertised nothing refuses nothing: an empty
    /// catalogue is not knowing what the CLI offers, and refusing on
    /// ignorance would drop names it does have.
    #[test]
    fn a_session_that_advertised_nothing_refuses_nothing() {
        let (ws, _rx, seat) = a_fleet();

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/compact".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "the core takes it: {dispatched:?}");
        let commands = ws.drain_test_dispatch_buffer();
        assert!(
            matches!(commands.as_slice(), [Command::Prompt { .. } | Command::PromptUnder { .. }]),
            "nothing is refused before the CLI has said what it has: {commands:?}",
        );
    }

    /// A name that only LOOKS like a forge command is the reader's prose, and
    /// reaches the model as they typed it.
    #[test]
    fn a_prompt_that_only_starts_like_a_forge_command_is_still_a_prompt() {
        let (ws, _rx, seat) = a_fleet();

        let dispatched = ws.dispatch_from_view(Command::Prompt {
            key: seat.clone(),
            text: "/newer please".to_owned(),
            attachments: Vec::new(),
        });

        assert!(dispatched.is_ok(), "the core takes it: {dispatched:?}");
        let commands = ws.drain_test_dispatch_buffer();
        assert!(
            matches!(
                commands.as_slice(),
                [Command::Prompt { text, .. } | Command::PromptUnder { text, .. }]
                    if text == "/newer please"
            ),
            "a near miss is prose: {commands:?}",
        );
    }

    /// The socket's entry, which is what `ViewSurface::dispatch` takes.
    #[test]
    fn a_views_entry_marks_the_frame_as_a_views() {
        let (ws, mut rx, seat) = a_fleet();

        let dispatched = ws.dispatch_from_view(a_prompt(&seat));

        assert!(dispatched.is_ok(), "precondition: the core took the prompt: {dispatched:?}");
        assert!(
            matches!(
                rx.try_recv(),
                Ok(SessionUpdate::ChatAppended { origin: Some(PromptOrigin::View), key, .. })
                    if key == seat
            ),
            "no view has drawn a send made over the socket, so every view has to",
        );
    }

    /// The delivery path, which already draws as an envelope turn of its own.
    #[test]
    fn a_delivery_emits_no_frame() {
        let (ws, mut rx, seat) = a_fleet();

        let _ = ws.dispatch_workspace_prompt(&seat, "run the morning summary".to_owned());

        assert!(
            rx.try_recv().is_err(),
            "a bare user turn beside a delivery's envelope turn draws the same words twice",
        );
    }

    /// A prompt the core refuses draws nothing anywhere.
    ///
    /// It never reached a model, so there is no turn to draw - and a frame for
    /// one would open a live turn in every view but the sender, while the
    /// refusal is written to the asking socket alone. Those views cannot learn
    /// why, so the turn bar spins for the life of the connection.
    #[test]
    fn a_refused_prompt_emits_no_frame() {
        // No intercept: the dispatch takes the real path and is refused, which
        // is the case this pins.
        let (ws, mut rx) = Workspace::testing_stub();
        let nowhere = SessionSlot::lead("TestOrg", "no-such-project");

        let refused = ws.dispatch(a_prompt(&nowhere));

        assert!(refused.is_err(), "precondition: the core holds no session here: {refused:?}");
        assert!(
            rx.try_recv().is_err(),
            "a refused prompt opened a turn in every view but the sender",
        );
    }
}
