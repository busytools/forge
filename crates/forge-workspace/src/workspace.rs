//! The orchestrator.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use forge_agent::AgentHandle;
use forge_agent::client::SessionLaunchSettings;
use forge_primitives::{PeerInflightStats, SDKSessionInfo};

use crate::mcp::peers::types::{
    AskChannel, CorrelationId, InflightAsk, WrappedKind, WrappedPrompt,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::Instrument;

use forge_gateway::AccountKey;
use forge_gateway::ProviderHost as _;

use crate::config::{LoadedConfig, LoadedProject, load_from_dir};
use crate::domain_session::DomainSession;
use crate::error::WorkspaceError;
use crate::protocol::{Command, DispatchError, SessionUpdate};
use crate::session_task::SessionTask;
use crate::spawn;
use crate::target::{ProjectKey, SessionSlot, SessionTarget};
use crate::views::{AccountLoadingRow, ProjectView, SessionView};

#[cfg(any(test, feature = "testing"))]
mod testing;

/// How often the background poller refreshes account usage. The
/// TUI's bottom panel and the gateway's account selection both read
/// from the cache this poll populates. 60 s upper-bounds how stale
/// the "which account has more headroom" decision can be while
/// staying clear of the OAuth usage endpoint's 429 throttle under
/// multi-instance polling - combined with per-account `last_error`
/// backoff (see `forge_gateway::AccountState`), transient 429s recover
/// naturally.
const USAGE_POLL_INTERVAL: Duration = Duration::from_secs(60);

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

/// Delegation block appended to a Lead session's system prompt.
///
/// Lead-only: `workers__spawn` refuses a worker caller, so a worker
/// given this block would be told to call a tool that rejects it.
///
/// Matches the shipped-prompt constants in `forge-agent`: one escaped
/// literal, no runtime assembly.
const LEAD_DELEGATION_PREAMBLE: &str = "\
You can delegate work to worker sessions via the \
mcp__forge__workers__ tools. These tools manage THIS project's worker \
sessions only. The peers__* family is a different one: it addresses \
other projects' agents (list / ask / tell) and never creates a worker \
in YOUR project - if you mean to spawn a worker, emit workers__spawn, \
never a peers call. Spawn one with \
workers__spawn(label=\"<name>\", charter=\"<its mission>\") - the charter \
is required and is what defines that worker; talk to it with \
workers__tell / workers__ask; list live workers with workers__list; \
revise a worker's stored charter or kicks with workers__update, which \
takes effect on its next restart. At most one live worker exists per \
label - if it already exists, message it instead of spawning again. \
Spawned workers are durable: they survive forge restarts and re-spawn \
automatically, resuming where they left off, until you explicitly \
despawn them with workers__despawn (or close their row in the Projects \
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChipInfo {
    /// Account `display_name` from forge.toml `[[accounts]]`.
    pub account_name: String,
    /// Render category: drives the chip's color + optional prefix
    /// glyph.
    pub state: SessionChipState,
}

/// Visual category for a session chip. The renderer maps these to
/// foreground colors + (for `Bailed` alone) a leading `⚠ ` glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// The dedupe key for a delivered Slack message: one destination is
/// (project, owner, conversation, ts), so a lead and a worker in one
/// project never starve each other as "already delivered".
pub(crate) type SlackDeliveryKey = (String, Option<String>, String, String);

/// Multi-session orchestrator. Owns the project catalog snapshot
/// loaded from `<config_dir>/forge.toml` and the pool of currently
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
    /// Fan-in [`SessionUpdate`] sender. Cloned and handed to TUI-side
    /// modules (slash executors, plugin install, service-status check)
    /// via [`Self::update_sender`] so they can emit presentation
    /// events on the same channel TUI subscribes to.
    /// `pub(crate)` so the impl block in [`crate::crons`] can reach it.
    pub(crate) update_tx: mpsc::UnboundedSender<SessionUpdate>,
    /// Single-take slot holding the matching receiver. [`Self::subscribe`]
    /// pops it on first call; subsequent calls return `None`.
    update_rx_slot: Mutex<Option<mpsc::UnboundedReceiver<SessionUpdate>>>,
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
    /// Shared [`DomainSession`] handles, one per active `SessionTask`.
    /// `pub(crate)` so crate-internal spawn and delivery paths can
    /// reach a session's `DomainSession` directly.
    pub(crate) domain_handles: Mutex<HashMap<SessionSlot, Arc<Mutex<DomainSession>>>>,
    /// Wire-shape state for in-flight peer-coordination asks
    /// (`mcp__forge__peers__ask_agent`). One entry per outstanding ask
    /// keyed by [`CorrelationId`]. Registered by
    /// [`mcp::peers::facade::WorkspaceFacade::register_inflight_ask`]
    /// when a caller's `ask_agent` tool fires; removed on successful
    /// reply (`complete_inflight_ask`) or target-failure
    /// (`expire_inflight_ask_failed`).
    ///
    /// There is no timeout machinery - asks live until reply or
    /// crash. The peer-mcp v1 brainstorm had a 30-min timer + late-
    /// reply tagging but the user opted to drop both: peers are
    /// expected to respond promptly, and a forever-pending entry is
    /// cheaper than a stale-notification bug class.
    pub(crate) inflight_asks: Mutex<HashMap<CorrelationId, InflightAsk>>,
    /// Per-session counters of peer-message activity. Mutated by
    /// [`mcp::peers::facade::WorkspaceFacade::bump_inflight_stats`]
    /// whenever a peer ask is registered / replied / timed out /
    /// delivery-failed. Read by `list_peers` and `whoami`. Drives
    /// `SessionUpdate::PeerInflightStatsChanged` which the TUI
    /// reducer turns into sidebar peer-activity badges.
    pub(crate) peer_stats: Mutex<HashMap<SessionSlot, PeerInflightStats>>,
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
    /// loaded from `cron.toml` at boot and persisted back after
    /// every mutation - create/delete, the scheduler's fire-advance, and
    /// boot catch-up - through the one [`Workspace::with_crons_mut`] path.
    /// The single-instance guard makes this the only process touching the
    /// file, so this mutex alone serialises writes.
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
    /// draft id and carrying the session that asked. The sender is what
    /// the blocked `slack__post` handler awaits; removing the entry is
    /// what answers it. The owner is stored beside it so an answer is
    /// only ever applied by the session it was addressed to.
    pub(crate) slack_drafts:
        Mutex<HashMap<uuid::Uuid, (SessionSlot, tokio::sync::oneshot::Sender<bool>)>>,
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
    /// The project's worker cap is reached: `live` workers are up
    /// against a cap of `cap`.
    AtCap { live: usize, cap: usize },
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
    update_tx: &mpsc::UnboundedSender<SessionUpdate>,
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
    update_tx: mpsc::UnboundedSender<SessionUpdate>,
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
        false, // hide worker-tagged sessions from default catalog
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
/// without durable crons, subscriptions or dynamic workers this session
/// (hard rule #14: no cwd fallback).
fn open_db(app_support: &Path) -> Option<crate::store::Db> {
    if let Err(error) = std::fs::create_dir_all(app_support) {
        tracing::warn!(
            target: "forge_workspace::workspace",
            %error,
            path = %app_support.display(),
            "creating the app-support dir failed; durable crons, subscriptions and dynamic workers will not persist",
        );
        return None;
    }
    match crate::store::Db::open(&app_support.join("db.redb")) {
        Ok(db) => Some(db),
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "opening the redb store failed; durable crons, subscriptions and dynamic workers will not persist",
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

impl Workspace {
    /// Builds a Workspace, kicks off the background catalog scan, and
    /// loads `<config_dir>/forge.toml`. Errors if `forge.toml` is
    /// missing or malformed (e.g. no `[[orgs]]` entries, no
    /// `[[orgs.projects]]` entries, unknown account references). No
    /// Agents are spawned on success. The session catalog starts empty
    /// and fills when the scan lands - see [`Workspace::start_catalog_scan`].
    pub fn new(config_dir: PathBuf) -> Result<Self, WorkspaceError> {
        Self::new_impl(config_dir, None, true)
    }

    /// Like [`Workspace::new`] but puts forge's whole app-support base -
    /// the redb store and the single-instance lock - under a tempdir
    /// inside `config_dir` rather than the real machine
    /// `app_support_dir`, so tests never touch the user's durable store
    /// or contend for their live lock. The catalog scan does NOT
    /// auto-start; tests opt in via [`Workspace::start_catalog_scan`]
    /// so the catalog stays empty until a fixture asks for it.
    #[cfg(any(test, feature = "testing"))]
    pub fn new_for_test(config_dir: PathBuf) -> Result<Self, WorkspaceError> {
        let app_support = config_dir.join("app-support");
        let workspace = Self::new_impl(config_dir, Some(app_support), false)?;
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

    /// Shared constructor body. `app_support` supplies the app-support
    /// base dir; `None` resolves the real machine `app_support_dir` and
    /// degrades to no lock and no durable store on failure (hard rule
    /// #14: no cwd fallback).
    fn new_impl(
        config_dir: PathBuf,
        app_support: Option<PathBuf>,
        auto_start_scan: bool,
    ) -> Result<Self, WorkspaceError> {
        let mut config = load_from_dir(&config_dir)?;

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

        // The store's runtime spinner override (set via `/spinner`) wins
        // over the hand-authored forge.toml `[ui] spinner` default.
        // Folding it into `config.ui` here means `ui_settings()` returns
        // the effective style.
        config.ui.spinner = crate::ui::resolve_spinner(state.spinner, config.ui.spinner);

        let gateway_port = config.gateway_port;
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let (kick_dispatcher_tx, kick_dispatcher_rx) = mpsc::unbounded_channel::<KickRequest>();
        let config_dictate = config.dictate.clone();
        let db = Arc::new(Mutex::new(db));
        let catalog_loaded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let catalog_scan_started = std::sync::atomic::AtomicBool::new(false);
        if auto_start_scan {
            spawn_background_catalog_scan(
                &catalog,
                &db,
                &config_dir,
                &update_tx,
                &catalog_loaded,
                &catalog_scan_started,
            );
        }
        let workspace = Self {
            config_dir,
            config,
            catalog,
            pool: Mutex::new(HashMap::new()),
            accounts,
            gateway,
            gateway_ready: std::sync::atomic::AtomicBool::new(false),
            gateway_port,
            gateway_url: Mutex::new(None),
            gateway_bind_error: Mutex::new(None),
            dictate: Arc::new(crate::dictate::DictateState::new(&config_dictate)),
            dictate_runtime: Mutex::new(crate::dictate::DictateRuntime::default()),
            dictate_device_pick: Mutex::new(None),
            update_tx,
            update_rx_slot: Mutex::new(Some(update_rx)),
            command_senders: Mutex::new(HashMap::new()),
            live_workers: Mutex::new(HashMap::new()),
            domain_handles: Mutex::new(HashMap::new()),
            inflight_asks: Mutex::new(HashMap::new()),
            peer_stats: Mutex::new(HashMap::new()),
            review_origin: Mutex::new(HashMap::new()),
            review_activity: Mutex::new(HashMap::new()),
            usage_poller_started: std::sync::atomic::AtomicBool::new(false),
            cron_scheduler_started: std::sync::atomic::AtomicBool::new(false),
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
        };
        if workspace.db.lock().is_none() {
            // One user-visible notice for the whole best-effort-persist
            // class (spinner override, durable crons, subscriptions): the
            // store is gone this run, so every one of those warns would
            // otherwise fire per-op into the log only.
            let _ = workspace.update_tx.send(SessionUpdate::ServiceStatus {
                severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                message: "Machine-local store unavailable this run; crons, Gotify and Slack subscriptions and the spinner override will not persist".to_owned(),
            });
        }
        Ok(workspace)
    }

    /// Effective `[ui]` settings. All fields have defaults so callers
    /// can use the result without worrying about whether the section
    /// was present in the config file. `spinner` carries the resolved
    /// active style: the store's runtime override (set via `/spinner`)
    /// if present, else the forge.toml `[ui] spinner` default. Cheap
    /// clone - the struct is shallow.
    pub fn ui_settings(&self) -> crate::ui::UiSettings {
        self.config.ui.clone()
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

    /// Persist `style` as the runtime spinner override in the machine-
    /// local store (never touches the hand-authored forge.toml). The next
    /// boot's `Workspace::new` layers it over the forge.toml `[ui]
    /// spinner` default. Called by the `/spinner` picker (enter-apply) and
    /// the direct `/spinner <name>` path; the in-session active style
    /// lives on the TUI's `App::spinner_style`, so this write only affects
    /// subsequent launches. A no-op with a warn when the store is closed.
    pub fn persist_spinner(&self, style: crate::ui::SpinnerStyle) {
        if let Some(db) = self.db.lock().as_ref() {
            crate::account_cache::store_spinner(db, Some(style));
        } else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                "store unavailable; the /spinner override will not persist across restart",
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
        // Carry the row's provenance the same way: the tag-write
        // rollback runs long after this spawn returned and needs to know
        // whether the row it would delete is this spawn's to take.
        domain_arc.lock().spawn_wrote_row =
            matches!(role, crate::protocol::SpawnRole::Worker { wrote_row: true, .. });

        // Build the per-session `forge` MCP server. ONE server name;
        // tool surface depends on whether this spawn is for a project
        // lead or a worker. Leads see peers + workers (cross-project
        // coordination is a lead-only role); workers see workers
        // only. See `crate::mcp::SessionKind` for the rationale.
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
            let review_facade = crate::mcp::review::facade::ProdReviewFacade::from_arc(self);
            let cron_facade = crate::mcp::cron::facade::ProdCronFacade::from_arc(self);
            let gotify_facade = crate::mcp::gotify::facade::ProdGotifyFacade::from_arc(self);
            let slack_facade = crate::mcp::slack::facade::ProdSlackFacade::from_arc(self);
            let tasks_facade = crate::mcp::tasks::facade::ProdTasksFacade::from_arc(self);
            crate::mcp::build_forge_server(
                workspace_facade,
                worker_facade,
                review_facade,
                cron_facade,
                gotify_facade,
                slack_facade,
                tasks_facade,
                session_slot.clone(),
                session_kind,
            )
        };

        let handle = forge_agent::Agent::spawn(
            account_dir.clone(),
            Some(account_key.0.clone()),
            vec![("forge".to_owned(), forge_server)],
            session_env,
        );
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

    /// The inputs a `/dictate` device pick can offer, plus the
    /// configured pin. Blocking (the cpal device walk), so the TUI
    /// calls it from a spawned task, never the render thread.
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
    pub fn start_dictate_preflight(self: &Arc<Self>) {
        let settings = self.config.dictate.clone();
        if !settings.enabled {
            return;
        }
        let state = Arc::clone(&self.dictate);
        let updates = self.update_sender();
        let span = tracing::info_span!("dictate_preflight");
        tokio::spawn(
            async move {
                crate::dictate::run_dictate_preflight(settings, state.clone()).await;
                // `run_dictate_preflight` parks the engine in the state
                // only on success, and every failure path ends the run,
                // so a held engine is the whole availability signal.
                if state.engine.lock().is_some() {
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
    /// loops on `recv()`, calls [`Self::dispatch`] for each request,
    /// then sleeps `KICK_DISPATCH_INTERVAL` before the next pull.
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
                        workspace.dispatch_workspace_prompt(&session_key, req.prompt_body)
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
    /// The CLI's task tools are denied unconditionally, because a
    /// respawn's settings are built by the TUI and carry no spawn-time
    /// flags - so `/new` and `/resume` are exactly where those denials
    /// would otherwise come back.
    pub(crate) fn stamp_respawn_overrides(
        &self,
        slot: &SessionSlot,
        session_id: &str,
        launch_settings: &mut SessionLaunchSettings,
    ) {
        crate::spawn::apply_disallowed_task_tools(launch_settings);
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
    /// the status echo and `workers__list` name the occupant that is
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
    /// it live. `None` for a slot with no pooled session.
    pub(crate) fn running_session_id_for(&self, slot: &SessionSlot) -> Option<String> {
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
    pub(crate) fn session_last_activity(
        &self,
        slot: &SessionSlot,
    ) -> Option<std::time::SystemTime> {
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

    /// Internal accessor for the SessionUpdate fan-in sender. Used
    /// by `spawn.rs` to emit `Spawning` / `ConnectionFailed` /
    /// `FatalError` from the App-level handlers.
    pub(crate) fn update_tx(&self) -> &mpsc::UnboundedSender<SessionUpdate> {
        &self.update_tx
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

    /// Single-take fan-in receiver for [`SessionUpdate`]s. Returns
    /// `None` on subsequent calls (and logs at error level so a
    /// second-subscriber programming error doesn't disappear into
    /// silent data loss). forge-tui's main event loop owns the
    /// returned `mpsc::UnboundedReceiver` and reads `SessionUpdate`
    /// envelopes directly - this is the sole event source the App
    /// consumes.
    pub fn subscribe(&self) -> Option<mpsc::UnboundedReceiver<SessionUpdate>> {
        if let Some(rx) = self.update_rx_slot.lock().take() {
            Some(rx)
        } else {
            tracing::error!(
                target: "forge_workspace::workspace",
                "Workspace::subscribe called after the receiver was already taken - second subscriber would silently receive nothing"
            );
            None
        }
    }

    /// Clone the [`SessionUpdate`] sender. TUI-side async tasks
    /// (plugin inventory refresh, usage refresh, slash executors,
    /// service-status check, the input-submit cancel-emit path) hold
    /// a clone so they can forward state into the App's event loop
    /// the same way the workspace's `SessionTask`s do. Cloned at App
    /// construction time and stored on `App.update_tx`.
    pub fn update_sender(&self) -> mpsc::UnboundedSender<SessionUpdate> {
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
    pub fn dispatch_workspace_prompt(
        self: &Arc<Self>,
        key: &SessionSlot,
        text: String,
    ) -> Result<(), DispatchError> {
        // Busy is captured before the dispatch: dispatching first would
        // read the turn_pending stamp the dispatch itself just set.
        // Signalling only on success keeps a failed dispatch (the
        // log-only failure sites never emit a TurnError) from
        // stranding a count nothing clears. Whether the re-open-gap
        // residual signals at all depends on a session_state_changed
        // mirror being present, so it is CLI-version-dependent.
        let busy = self.domain_session_for(key).is_some_and(|d| d.lock().turn_in_flight());
        let result =
            self.dispatch(Command::Prompt { key: key.clone(), text, attachments: Vec::new() });
        if busy && result.is_ok() {
            let _ = self
                .update_sender()
                .send(SessionUpdate::PromptQueuedWhileBusy { key: key.clone() });
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
    pub fn dispatch(self: &Arc<Self>, mut cmd: Command) -> Result<(), DispatchError> {
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
                // whose wire-lagged `Running` echo hasn't landed yet.
                if matches!(cmd, Command::Prompt { .. })
                    && let Some(domain) = self.domain_session_for(&key)
                {
                    domain.lock().turn_pending = true;
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
                        correlation_id = %wrapped.correlation_id,
                    );
                    let _enter = span.enter();
                    spawn::handle_deliver_peer_prompt(self, caller, target_project, wrapped);
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
                        spawn::WorkerSpawnArgs { label, charter, kick, resume_kick, interactive },
                        spawned_by,
                        resume_existing.as_deref(),
                        from_boot_respawn,
                        return_to,
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
                    spawn::handle_despawn_worker(self, &project_key, &label, force, respond);
                }
                Command::DeliverWorkerPrompt { caller, project_key, target_label, wrapped } => {
                    let span = tracing::info_span!(
                        "deliver_worker_prompt",
                        project = %project_key.as_str(),
                        label = %target_label,
                        correlation_id = %wrapped.correlation_id,
                    );
                    let _enter = span.enter();
                    spawn::handle_deliver_worker_prompt(
                        self,
                        caller,
                        &project_key,
                        &target_label,
                        wrapped,
                    );
                }
                Command::DeliverWorkerPromptToLead { caller, target_lead_key, wrapped } => {
                    let span = tracing::info_span!(
                        "deliver_worker_prompt_to_lead",
                        slot = %target_lead_key.display(),
                        correlation_id = %wrapped.correlation_id,
                    );
                    let _enter = span.enter();
                    spawn::handle_deliver_worker_prompt_to_lead(
                        self,
                        caller,
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
                    self.resolve_slack_draft(id, &key, approved);
                }
                Command::OpenUrl { url } => {
                    let span = tracing::info_span!("open_url", url = %url);
                    let _enter = span.enter();
                    spawn::handle_open_url(self, url);
                }
                Command::DictateStart { key } => {
                    let ws = Arc::clone(self);
                    tokio::spawn(async move {
                        crate::dictate::handle_dictate_start(&ws, key).await;
                    });
                }
                Command::DictateStop { key, submit } => {
                    let ws = Arc::clone(self);
                    tokio::spawn(async move {
                        crate::dictate::handle_dictate_stop(&ws, &key, submit).await;
                    });
                }
                // User-action store writes routed through the command
                // bus (MVVM: one channel pair). Synchronous inline
                // handlers - the writes are local redb operations, and
                // the TUI has already applied its optimistic state.
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
                Command::PersistSpinner { style } => {
                    let span = tracing::info_span!("persist_spinner", style = %style.key());
                    let _enter = span.enter();
                    self.persist_spinner(style);
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
                    let _ = respond.send(self.upsert_review_thread(&project, &branch, thread));
                }
                Command::SubmitReview { project, branch, summary, thread_ids, origin, respond } => {
                    let span = tracing::info_span!(
                        "submit_review",
                        project = %project,
                        branch = %branch,
                        threads = thread_ids.len(),
                    );
                    let _enter = span.enter();
                    let _ = respond.send(self.submit_review(
                        &project,
                        &branch,
                        summary,
                        &thread_ids,
                        origin,
                    ));
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
            // takes `--worktree`, so only a resume is stranded.
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
                interactive: worker.interactive.unwrap_or(false),
                from_boot_respawn: true,
                return_to: tx,
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
                    })
                    .collect();
                (project.clone(), states)
            })
            .collect()
    }

    /// What `entry`'s session is doing right now - the axis
    /// `WorkerLiveness` does not answer, since it stops moving once the
    /// worker connects.
    ///
    /// A pending interaction outranks the turn it is blocking:
    /// `Attention` is the state a lead has to act on, and reporting the
    /// blocked worker as `Running` is what let the deadlock stay
    /// invisible.
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
        let Some(domain) = self.domain_session_for(&entry.slot) else {
            return L::Sleeping;
        };
        let guard = domain.lock();
        // A permission request only exists during a turn, so with no
        // turn there is nothing to be blocked on - a slot outliving its
        // turn (busytools/forge#672) is incoherent state rather than a
        // worker awaiting input, and must not read as `Attention`.
        if !guard.turn_in_flight() {
            return L::Idle;
        }
        // A turn is in flight, so ask whether it can advance on its own.
        // `RequiresAction` is the CLI naming its own block; a held slot
        // is forge naming it. Either way a human has to move first, and
        // calling that `Running` is what makes a blocked worker
        // invisible. This arm is reachable only because
        // `turn_in_flight()` counts `RequiresAction` as in-flight:
        // drop it from that OR and a `RequiresAction` session falls
        // through the gate above to `Idle` instead.
        if matches!(
            guard.runtime_state,
            Some(forge_primitives::RuntimeSessionState::RequiresAction)
        ) || !guard.pending_interactions.is_empty()
        {
            L::Attention
        } else {
            L::Running
        }
    }

    /// `entry` projected to the wire shape with `activity` derived.
    /// This is the `workers__list` projection; `WorkerEntry::to_status`
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
    /// reconnect re-spawn racing a manual `workers__spawn`, say) can't
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
        let mut workers = self.live_workers.lock();
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
    /// 1. The sessions catalog ([`Self::session_cwd_for`]). Leads only:
    ///    the boot scan hides worker-tagged sessions and the Connected
    ///    handler skips the catalog mirror for workers, so a worker
    ///    never has a row to read.
    /// 2. The worker registry (`live_workers` via
    ///    [`Self::worker_lookup_for_session`]), composed against the
    ///    project's `forge.toml` path by [`worker_tag_dir`] - the
    ///    worktree for a git worker, the project root otherwise. This
    ///    is the authoritative source for every worker.
    ///
    /// `None` leaves the caller to decide what an unknown cwd means:
    /// [`Self::resume_cwd_for_slot`] hands claude an empty cwd,
    /// while the review MCP reports `SessionCwdUnknown` to the caller.
    ///
    /// [`worker_tag_dir`]: crate::mcp::workers::types::worker_tag_dir
    pub(crate) fn cwd_for_session(&self, session_key: &SessionSlot) -> Option<String> {
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
    ///   [`WorkerLiveness::Failed`] with the first line of the
    ///   message as the diagnostic. The Projects pane renders the
    ///   worker as a red `✕` with a DIM sub-row carrying the
    ///   diagnostic, so a stuck-Spawning-forever case becomes
    ///   visible instead of silently disappearing.
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
        // Any ask already routed at this worker dies with the spawn -
        // buffered asks were never delivered, so the target_session
        // predicate in expire_target_inflight can't catch them.
        self.expire_inflight_for_closed_worker(&project_key, &entry.label);
        // Classify against the entry's recorded is_git_repo_at_spawn
        // flag - same heuristic the sync workers__spawn path uses.
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
            // picks up the bracketed prefix via peer_block::detect_inbound.
            let lead_slot = entry.spawned_by.clone();
            let pool_has_lead = self.pool.lock().contains_key(&lead_slot);
            if pool_has_lead {
                let wrapped = WrappedPrompt {
                    correlation_id: CorrelationId::new_tell(),
                    kind: WrappedKind::WorkerSpawnFailedNotice,
                    channel: AskChannel::Workers,
                    sender_name: entry.label.clone(),
                    sender_org: String::new(),
                    body: reason.clone(),
                };
                if let Err(err) = self.dispatch_workspace_prompt(&lead_slot, wrapped.to_prose()) {
                    tracing::warn!(
                        target: "forge_workspace::worker_async_failure",
                        project = %project_key.as_str(),
                        label = %entry.label,
                        error = ?err,
                        "WorkerSpawnFailedNotice dispatch to lead failed",
                    );
                }
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

    /// Stamp the session that received an ask's `IncomingPlus1` onto
    /// its `InflightAsk`, paired with every Question delivery so a
    /// later `expire_inflight_ask_failed` can clear that session's
    /// incoming badge (no-op once the ask completes).
    pub(crate) fn stamp_inflight_target(&self, id: &CorrelationId, target: &SessionSlot) {
        if let Some(ask) = self.inflight_asks.lock().get_mut(id) {
            ask.target_session = Some(target.clone());
        }
    }

    /// Expire every in-flight ask whose target session is the one
    /// closing. Called when:
    /// - `AgentEvent::ConnectionFailed` arrives for a target's bridge
    ///   (target's claude subprocess crashed or failed to spawn)
    /// - A `SessionTask::drop` fires (target's session was closed by
    ///   any reason - user close, lifecycle terminate, panic)
    ///
    /// Walks `inflight_asks` for entries stamped with the closing
    /// session (`target_session`, set at delivery - covers workers,
    /// whose composite `target_project` never matches a plain project
    /// name) or whose `target_project` matches the closing session's
    /// project, and dispatches the failure dual-path notification for
    /// each (PeerAskFailed UI state + Command::Prompt with
    /// DeliveryFailureNotice wrapper to caller).
    ///
    /// Idempotent. Safe to call from a Drop impl via Weak<Workspace>.
    pub(crate) fn expire_target_inflight(
        self: &Arc<Self>,
        closing_key: &SessionSlot,
        reason: crate::mcp::peers::types::PeerFailureReason,
    ) {
        // The project the closing session belongs to, named by its slot
        // rather than looked up: a worker never enters the catalog
        // mirror, and the target_session predicate below still catches
        // its delivered asks.
        let project_name = self
            .list_projects()
            .into_iter()
            .find(|v| v.org == closing_key.org() && v.name == closing_key.project())
            .map(|v| v.name);

        // Snapshot the IDs to expire. Holding the inflight_asks lock
        // across the dispatch loop below would risk re-entrancy via
        // bump_inflight_stats. Take a copy + release the lock.
        let ids_to_expire: Vec<CorrelationId> = {
            let asks = self.inflight_asks.lock();
            asks.iter()
                .filter(|(_, ask)| {
                    ask.target_session.as_ref() == Some(closing_key)
                        || project_name.as_ref().is_some_and(|name| ask.target_project == *name)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };

        for id in ids_to_expire {
            self.expire_inflight_ask_failed(&id, reason);
        }
    }

    /// Expire an in-flight ask because the target session crashed
    /// or was closed while the ask was open. Dispatches a
    /// `DeliveryFailureNotice` wrapper to the caller so its LLM
    /// learns the ask died. Idempotent.
    pub(crate) fn expire_inflight_ask_failed(
        self: &Arc<Self>,
        id: &CorrelationId,
        reason: crate::mcp::peers::types::PeerFailureReason,
    ) {
        let ask = {
            let mut asks = self.inflight_asks.lock();
            asks.remove(id)
        };
        let Some(ask) = ask else {
            tracing::trace!(
                target: "forge_workspace::workspace",
                correlation_id = %id,
                "expire_inflight_ask_failed: entry already gone"
            );
            return;
        };

        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(self);
        facade.bump_inflight_stats(
            &ask.caller,
            crate::mcp::peers::facade::PeerStatsDelta::DeliveryFailedPlus1,
        );
        facade.bump_inflight_stats(
            &ask.caller,
            crate::mcp::peers::facade::PeerStatsDelta::OutgoingMinus1,
        );
        // If the ask reached a target (its incoming was bumped at
        // delivery), clear that side too - otherwise the target's `N↓`
        // stays lit for an ask that will never be answered.
        if let Some(target) = &ask.target_session {
            facade.bump_inflight_stats(
                target,
                crate::mcp::peers::facade::PeerStatsDelta::IncomingMinus1,
            );
        }

        let target_org = self
            .list_projects()
            .into_iter()
            .find(|p| p.name == ask.target_project)
            .map_or_else(|| "?".to_owned(), |p| p.org);

        // Body carries the human-readable failure reason - caller
        // chat block surfaces it underneath the bracket header.
        let body = match &reason {
            crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed => {
                "target session connection lost".to_owned()
            }
        };

        let caller_notice = WrappedPrompt {
            correlation_id: id.clone(),
            kind: WrappedKind::DeliveryFailureNotice,
            channel: ask.channel,
            sender_name: ask.target_project.clone(),
            sender_org: target_org,
            body,
        };
        // The CLI never echoes stdin-injected prompts back, so paint the
        // visible notice block ourselves before the LLM-side dispatch.
        crate::spawn::push_peer_user_turn_into_chat(self, &ask.caller, &caller_notice);
        if let Err(err) = self.dispatch_workspace_prompt(&ask.caller, caller_notice.to_prose()) {
            tracing::warn!(
                target: "forge_workspace::workspace",
                correlation_id = %id,
                error = ?err,
                "expire_inflight_ask_failed: caller notice dispatch failed (caller closed?)"
            );
        }
    }

    /// Deliver a Reply straight to the asker's session, bypassing
    /// name/label resolution. The asker is identified by `SessionSlot`
    /// (a worker asker has no addressable project name), so this
    /// by-session path is load-bearing for closing a cross-agent ask.
    /// Confirms the caller session is still live before dispatching.
    /// Shared by the peers + workers facades.
    pub(crate) fn deliver_reply_to_caller(
        self: &Arc<Self>,
        caller: &SessionSlot,
        reply: &WrappedPrompt,
    ) -> Result<(), crate::mcp::peers::facade::ReplyDeliverError> {
        use crate::mcp::peers::facade::ReplyDeliverError;
        if !self.pool.lock().contains_key(caller) {
            return Err(ReplyDeliverError::CallerSessionGone);
        }
        // The CLI never echoes stdin-injected prompts back, so paint the
        // visible reply block ourselves before the LLM-side dispatch.
        crate::spawn::push_peer_user_turn_into_chat(self, caller, reply);
        if let Err(err) = self.dispatch_workspace_prompt(caller, reply.to_prose()) {
            tracing::warn!(
                target: "forge_workspace::workspace",
                correlation_id = %reply.correlation_id,
                error = ?err,
                "deliver_reply_to_caller: dispatch failed (caller closed?)"
            );
            return Err(ReplyDeliverError::CallerSessionGone);
        }
        Ok(())
    }

    /// Expire every in-flight ask whose `target_project` matches the
    /// `<project_key>::<label>` composite for a closed worker.
    ///
    /// Worker-bound asks stamp this composite onto `InflightAsk.target_project`
    /// (see `crate::mcp::workers::worker_target_project_key`); when a
    /// worker is closed via `handle_close_worker`, the per-session
    /// connection-failed expiry (`expire_target_inflight`) would never
    /// match because that path looks up the project by session-key
    /// presence in the catalog and matches on the project's plain
    /// name. The composite key path covers worker-bound traffic
    /// specifically.
    ///
    /// Each matching ask is rolled through `expire_inflight_ask_failed`
    /// so the caller's LLM receives the same `DeliveryFailureNotice`
    /// turn it would for any other target loss.
    pub(crate) fn expire_inflight_for_closed_worker(
        self: &Arc<Self>,
        project_key: &crate::ProjectKey,
        label: &str,
    ) {
        let composite = crate::mcp::workers::worker_target_project_key(project_key.as_str(), label);
        let ids_to_expire: Vec<CorrelationId> = {
            let asks = self.inflight_asks.lock();
            asks.iter()
                .filter(|(_, ask)| ask.target_project == composite)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids_to_expire {
            self.expire_inflight_ask_failed(
                &id,
                crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
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

    #[test]
    fn persist_spinner_writes_the_redb_override() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        ws.persist_spinner(crate::ui::SpinnerStyle::Ember);

        let guard = ws.db.lock();
        let db = guard.as_ref().expect("db installed");
        assert_eq!(
            crate::store::state::spinner(db).expect("read spinner"),
            Some(crate::ui::SpinnerStyle::Ember),
            "persist_spinner writes the override into the store",
        );
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
    fn boot_load_reads_the_redb_spinner_override() {
        // Stands in for the removed connect.rs override test: a persisted
        // redb spinner override is what account_cache::load returns, so
        // the boot fold layers it over the forge.toml default. Kept off
        // the real machine db (issue #392) via a tempdir store + config dir.
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        let guard = ws.db.lock();
        let db = guard.as_ref().expect("db installed");
        crate::store::state::set_spinner(db, Some(crate::ui::SpinnerStyle::Ember)).expect("set");
        assert_eq!(
            crate::account_cache::load(db).spinner,
            Some(crate::ui::SpinnerStyle::Ember),
            "load returns the persisted redb override, which the boot fold wins with",
        );
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
        ws.record_worker_row(&key, "steward", "steward-uuid", "c", None, None, false, true)
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
        )
        .expect("seed the row this test then updates");
        let stored = |ws: &Arc<Workspace>| {
            ws.worker_rows_for_project(&project)
                .into_iter()
                .find(|w| w.label == "steward")
                .expect("row present")
        };

        assert!(
            ws.update_worker_row(&project, "steward", Some("new charter".to_owned()), None, None)
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
            )
            .expect("second update succeeds"),
        );
        let row = stored(&ws);
        assert_eq!(row.charter.as_deref(), Some("new charter"), "the earlier update survived");
        assert_eq!(row.kick.as_deref(), Some("new kick"));
        assert_eq!(row.resume_kick.as_deref(), Some("new resume"));

        assert!(
            !ws.update_worker_row(&project, "ghost", Some("c".to_owned()), None, None)
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

    /// The `workers__despawn` path (`handle_despawn_worker` ->
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
            .record_worker_row(&project, "reviewer", "id", "charter", None, None, false, false)
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
            crate::spawn::deliver_cron_prompt(&ws, "cronlead", None, "morning".to_owned(), false);
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        assert!(
            !drain_updates(&mut rx)
                .iter()
                .any(|u| matches!(u, SessionUpdate::PromptQueuedWhileBusy { .. })),
            "an idle cron fire must not signal PromptQueuedWhileBusy",
        );

        ws.domain_session_for(&lead_key).expect("domain").lock().turn_pending = true;
        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, "cronlead", None, "again".to_owned(), false);
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
                crate::protocol::Command::Prompt { key, .. } if *key == lead_key
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
                crate::protocol::SessionUpdate::SlackMessageAppended { key, prose }
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
            dispatched.iter().all(|cmd| !matches!(cmd, crate::protocol::Command::Prompt { .. })),
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
            dispatched.iter().all(|cmd| !matches!(cmd, crate::protocol::Command::Prompt { .. })),
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

    /// The review/spinner/close store writes route through the command
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
                respond: respond_tx,
            })
            .expect("dispatch");
        assert!(
            respond_rx.try_recv().expect("response present"),
            "an open store confirms the upsert on the responder"
        );

        // The spinner override persists through its variant too.
        workspace
            .dispatch(Command::PersistSpinner { style: crate::ui::SpinnerStyle::Star })
            .expect("dispatch");
        let db = workspace.db.lock();
        let stored = crate::store::state::spinner(db.as_ref().expect("db")).expect("read spinner");
        assert_eq!(stored, Some(crate::ui::SpinnerStyle::Star));
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
        let (update_tx, _task_update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let task = crate::session_task::SessionTask {
            key: key.clone(),
            handle: pooled_handle,
            command_rx,
            domain,
            update_tx,
            connected_once: true,
            workspace: Arc::downgrade(&workspace),
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

    fn forge_toml_with_two_projects() -> tempfile::TempDir {
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

[[orgs.projects]]
name = "gateway-backend"
path = "~/Projects/gateway-backend"
auto_start = false

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

    /// expire_inflight_ask_failed removes the entry from inflight_asks,
    /// fires the DeliveryFailed stat bump, and dispatches a
    /// DeliveryFailureNotice wrapper. Idempotent - a second call on the
    /// same id is a no-op.
    #[tokio::test]
    async fn expire_inflight_ask_failed_removes_entry_and_is_idempotent() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk, PeerFailureReason};
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));

        let caller = SessionSlot::from_str_for_test("caller-1");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: caller.clone(),
                target_project: "gateway-backend".to_owned(),
                target_session: None,
            },
        );
        assert!(workspace.inflight_asks.lock().contains_key(&id));

        workspace.expire_inflight_ask_failed(&id, PeerFailureReason::TargetConnectionFailed);
        assert!(!workspace.inflight_asks.lock().contains_key(&id), "entry removed after expire");

        // Idempotent - second call on the same id is a no-op.
        workspace.expire_inflight_ask_failed(&id, PeerFailureReason::TargetConnectionFailed);
        assert!(!workspace.inflight_asks.lock().contains_key(&id));
    }

    /// expire_inflight_ask_failed dispatches `PeerInflightStatsChanged`
    /// for the delivery-failed bookkeeping and removes the entry.
    /// expire_target_inflight is a thin loop over this per-id path
    /// (its predicate is pinned separately by
    /// `expire_target_inflight_matches_worker_asks_by_target_session`).
    #[tokio::test]
    async fn expire_inflight_ask_failed_dispatches_failure_notice() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk, PeerFailureReason};
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionSlot::from_str_for_test("caller-notice");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: caller.clone(),
                target_project: "gateway-backend".to_owned(),
                target_session: None,
            },
        );

        workspace.expire_inflight_ask_failed(&id, PeerFailureReason::TargetConnectionFailed);

        let mut saw_stats = false;
        while let Ok(update) = rx.try_recv() {
            if matches!(update, SessionUpdate::PeerInflightStatsChanged { .. }) {
                saw_stats = true;
            }
        }
        assert!(saw_stats, "PeerInflightStatsChanged fires for delivery_failed bump");
        assert!(!workspace.inflight_asks.lock().contains_key(&id));
    }

    /// expire_inflight_ask_failed paints the delivery-failure notice as
    /// a visible chat block: it emits a `PeerEnvelopeAppended` carrying
    /// the `DeliveryFailureNotice` for the caller's session, so a
    /// dead-target ask surfaces in the caller's chat, not just to its LLM.
    #[tokio::test]
    async fn expire_inflight_ask_failed_emits_peer_envelope_echo() {
        use crate::mcp::peers::types::{
            AskChannel, CorrelationId, InflightAsk, PeerFailureReason, WrappedKind,
        };
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionSlot::from_str_for_test("caller-notice-echo");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: AskChannel::Peers,
                caller: caller.clone(),
                target_project: "gateway-backend".to_owned(),
                target_session: None,
            },
        );

        workspace.expire_inflight_ask_failed(&id, PeerFailureReason::TargetConnectionFailed);

        let mut echo = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PeerEnvelopeAppended { key, wrapped } = update {
                echo = Some((key, wrapped));
            }
        }
        let (key, wrapped) = echo.expect("PeerEnvelopeAppended painted for the caller");
        assert_eq!(key, caller, "notice echo targets the caller's slot");
        assert_eq!(wrapped.correlation_id, id, "notice echo carries the ask id");
        assert!(
            matches!(wrapped.kind, WrappedKind::DeliveryFailureNotice),
            "notice echo carries the DeliveryFailureNotice kind",
        );
    }

    /// expire_target_inflight expires asks stamped with the closing
    /// session key even when the closing session resolves to no
    /// catalog project and target_project is a worker composite -
    /// the crash path for a worker that dies mid-ask.
    #[tokio::test]
    async fn expire_target_inflight_matches_worker_asks_by_target_session() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk, PeerFailureReason};
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));

        let worker_key = SessionSlot::from_str_for_test("worker-sess-1");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Workers,
                caller: SessionSlot::from_str_for_test("lead-1"),
                target_project: crate::mcp::workers::worker_target_project_key("forge", "builder"),
                target_session: Some(worker_key.clone()),
            },
        );

        workspace.expire_target_inflight(&worker_key, PeerFailureReason::TargetConnectionFailed);
        assert!(
            !workspace.inflight_asks.lock().contains_key(&id),
            "worker-bound ask expired via target_session match"
        );
    }

    /// A failed/expired ask must clear the TARGET's incoming badge, not
    /// just the caller's outgoing. Pre-fix `expire_inflight_ask_failed`
    /// only decremented the caller's outgoing, stranding the target's
    /// `N↓`; the `target_session` stamp lets expiry clear both sides.
    #[tokio::test]
    async fn expire_inflight_ask_failed_clears_target_incoming() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk, PeerFailureReason};
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));

        let caller = SessionSlot::from_str_for_test("asker");
        let target = SessionSlot::from_str_for_test("replier");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: caller.clone(),
                target_project: "gateway-backend".to_owned(),
                target_session: Some(target.clone()),
            },
        );
        // Mirror the runtime bumps: ask registered (caller outgoing +1),
        // then delivered (target incoming +1).
        {
            let mut stats = workspace.peer_stats.lock();
            stats.entry(caller.clone()).or_default().outgoing = 1;
            stats.entry(target.clone()).or_default().incoming = 1;
        }

        workspace.expire_inflight_ask_failed(&id, PeerFailureReason::TargetConnectionFailed);

        let stats = workspace.peer_stats.lock();
        assert_eq!(stats.get(&caller).map(|s| s.outgoing), Some(0), "caller outgoing cleared");
        assert_eq!(
            stats.get(&caller).map(|s| s.delivery_failed),
            Some(1),
            "caller delivery_failed bumped",
        );
        assert_eq!(
            stats.get(&target).map(|s| s.incoming),
            Some(0),
            "target incoming cleared on expiry (was stranded before the fix)",
        );
    }

    /// `stamp_inflight_target` records which session received an ask's
    /// `IncomingPlus1` so a later expiry can decrement that same key.
    #[test]
    fn stamp_inflight_target_records_target_session() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk};
        let (workspace, _rx) = Workspace::testing_stub();
        let id = CorrelationId::new_ask();
        let target = SessionSlot::from_str_for_test("replier");
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: SessionSlot::from_str_for_test("asker"),
                target_project: "gateway-backend".to_owned(),
                target_session: None,
            },
        );

        workspace.stamp_inflight_target(&id, &target);

        assert_eq!(
            workspace.inflight_asks.lock().get(&id).and_then(|a| a.target_session.clone()),
            Some(target),
            "target_session stamped for a later expiry to clear",
        );
    }

    /// Workspace::dispatch(Command::DeliverPeerPrompt) routes to the
    /// command channel without panicking. The full spawn-path handling
    /// is exercised in the spawn::handle_deliver_peer_prompt test.
    #[tokio::test]
    async fn deliver_reply_to_caller_routes_by_session_and_guards() {
        use crate::mcp::peers::facade::ReplyDeliverError;
        use crate::mcp::peers::types::{AskChannel, CorrelationId, WrappedKind, WrappedPrompt};
        let (ws, _rx) = Workspace::testing_stub();
        ws.enable_test_dispatch_intercept();

        let caller = SessionSlot::from_str_for_test("asker");
        let reply = WrappedPrompt {
            correlation_id: CorrelationId::new_tell(),
            kind: WrappedKind::Reply,
            channel: AskChannel::Workers,
            sender_name: "worker".to_owned(),
            sender_org: "worker in forge".to_owned(),
            body: "here's the answer".to_owned(),
        };

        // Happy path: caller live in the pool -> Ok
        // plus exactly one Command::Prompt to the caller carrying the prose.
        let (handle, _hrx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            caller.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("acct".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        assert_eq!(ws.deliver_reply_to_caller(&caller, &reply), Ok(()));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 1, "exactly one command dispatched");
        match &dispatched[0] {
            Command::Prompt { key, text, .. } => {
                assert_eq!(*key, caller, "prompt routed to the asker's session");
                assert_eq!(*text, reply.to_prose(), "prompt carries the reply prose");
            }
            other => panic!("expected Command::Prompt, got {other:?}"),
        }

        // Pool-miss: unknown caller -> CallerSessionGone, nothing dispatched.
        let ghost = SessionSlot::from_str_for_test("ghost");
        assert_eq!(
            ws.deliver_reply_to_caller(&ghost, &reply),
            Err(ReplyDeliverError::CallerSessionGone),
        );
        assert!(ws.drain_test_dispatch_buffer().is_empty(), "no dispatch on pool-miss");
    }

    /// deliver_reply_to_caller paints the visible peer block: it emits
    /// a `PeerEnvelopeAppended` for the caller's session carrying the
    /// reply, not merely the LLM-side `Command::Prompt`. The CLI never
    /// echoes stdin-injected prompts back, so this echo is the only
    /// signal that renders the inbound `[Reply ...]` chat block.
    #[tokio::test]
    async fn deliver_reply_to_caller_emits_peer_envelope_echo() {
        use crate::mcp::peers::types::{AskChannel, CorrelationId, WrappedKind, WrappedPrompt};
        let (ws, mut rx) = Workspace::testing_stub();
        ws.enable_test_dispatch_intercept();

        let caller = SessionSlot::from_str_for_test("asker");
        let reply = WrappedPrompt {
            correlation_id: CorrelationId::new_tell(),
            kind: WrappedKind::Reply,
            channel: AskChannel::Workers,
            sender_name: "worker".to_owned(),
            sender_org: "worker in forge".to_owned(),
            body: "here's the answer".to_owned(),
        };

        let (handle, _hrx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            caller.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("acct".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        assert_eq!(ws.deliver_reply_to_caller(&caller, &reply), Ok(()));

        let mut echo = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PeerEnvelopeAppended { key, wrapped } = update {
                echo = Some((key, wrapped));
            }
        }
        let (key, wrapped) = echo.expect("PeerEnvelopeAppended painted for the caller");
        assert_eq!(key, caller, "echo targets the caller's slot");
        assert_eq!(wrapped.correlation_id, reply.correlation_id, "echo carries the reply id");
        assert_eq!(wrapped.kind, reply.kind, "echo carries the Reply kind");
        assert_eq!(wrapped.body, reply.body, "echo carries the reply body");
    }

    /// Disk-backed workspace fixture shared by the per-project loop
    /// tests below. Returns the `Arc<Workspace>` plus the `TempDir`
    /// that holds the on-disk `forge.toml`; the caller must keep the
    /// `TempDir` alive (drop deletes the directory). Required because
    /// `expire_target_inflight` resolves the closing key's project via
    /// `list_projects()` (catalog-backed), so a fully-in-memory
    /// workspace would early-return.
    fn peer_mcp_workspace_fixture() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        (workspace, dir)
    }

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

    /// `expire_target_inflight` walks `inflight_asks`, finds entries
    /// whose `target_project` matches the closing key's project, and
    /// expires each via `expire_inflight_ask_failed`. Asks scoped to
    /// other projects' targets stay untouched.
    ///
    /// Covers the per-project loop wrapper that the per-id unit
    /// (`expire_inflight_ask_failed_dispatches_failure_notice`) sits
    /// underneath. The on-disk fixture is required because the loop
    /// resolves the closing session's project via `list_projects()`,
    /// which reads `forge.toml`; a fully-in-memory test would
    /// early-return.
    #[tokio::test]
    async fn expire_target_inflight_drains_only_targeted_asks() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk, PeerFailureReason};

        let (workspace, _dir) = peer_mcp_workspace_fixture();

        // Three inflight asks: two targeting gateway-backend (must
        // expire), one targeting forge (must survive).
        let caller_a = SessionSlot::from_str_for_test("caller-a");
        let caller_b = SessionSlot::from_str_for_test("caller-b");
        let caller_c = SessionSlot::from_str_for_test("caller-c");
        let id_a = CorrelationId::new_ask();
        let id_b = CorrelationId::new_ask();
        let id_c = CorrelationId::new_ask();
        {
            let mut asks = workspace.inflight_asks.lock();
            asks.insert(
                id_a.clone(),
                InflightAsk {
                    correlation_id: id_a.clone(),
                    channel: crate::mcp::peers::types::AskChannel::Peers,
                    caller: caller_a.clone(),
                    target_project: "gateway-backend".to_owned(),
                    target_session: None,
                },
            );
            asks.insert(
                id_b.clone(),
                InflightAsk {
                    correlation_id: id_b.clone(),
                    channel: crate::mcp::peers::types::AskChannel::Peers,
                    caller: caller_b.clone(),
                    target_project: "gateway-backend".to_owned(),
                    target_session: None,
                },
            );
            asks.insert(
                id_c.clone(),
                InflightAsk {
                    correlation_id: id_c.clone(),
                    channel: crate::mcp::peers::types::AskChannel::Peers,
                    caller: caller_c.clone(),
                    target_project: "forge".to_owned(),
                    target_session: None,
                },
            );
        }

        // Arm intercept so we can assert the per-id path fired a
        // Command::Prompt (DeliveryFailureNotice) for each targeted
        // ask without spinning up real caller session tasks.
        workspace.enable_test_dispatch_intercept();
        let closing_key = SessionSlot::lead("Default", "gateway-backend");
        workspace.expire_target_inflight(&closing_key, PeerFailureReason::TargetConnectionFailed);

        // Targeted asks are gone; the orthogonally-targeted ask survives.
        let asks = workspace.inflight_asks.lock();
        assert!(!asks.contains_key(&id_a), "ask targeting gateway-backend removed");
        assert!(!asks.contains_key(&id_b), "ask targeting gateway-backend removed");
        assert!(
            asks.contains_key(&id_c),
            "ask targeting forge survives, only the closing project's asks expire"
        );
        drop(asks);

        // One DeliveryFailureNotice Command::Prompt per expired ask,
        // routed back to each ask's caller. Sort by caller key before
        // comparing; HashMap iteration order isn't pinned.
        let buffered = workspace.drain_test_dispatch_buffer();
        let mut notice_callers: Vec<SessionSlot> = buffered
            .into_iter()
            .filter_map(|cmd| match cmd {
                crate::protocol::Command::Prompt { key, text, .. }
                    if text.contains("failed to deliver") =>
                {
                    Some(key)
                }
                _ => None,
            })
            .collect();
        notice_callers.sort_by_key(forge_primitives::SessionSlot::display);
        let mut expected_callers = vec![caller_a.clone(), caller_b.clone()];
        expected_callers.sort_by_key(forge_primitives::SessionSlot::display);
        assert_eq!(
            notice_callers, expected_callers,
            "DeliveryFailureNotice fired for exactly the two gateway-backend-targeted callers"
        );
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

    /// The whole point of the field: a worker that finished its turn is
    /// still `WorkerLiveness::Running`, so a lead polling `workers__list`
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
                d.pending_interactions
                    .insert("tool-1".to_owned(), PendingInteractionSlot::Permission(tx));
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
                .insert("tool-stranded".to_owned(), PendingInteractionSlot::Permission(tx));
        }

        assert_eq!(
            ws.worker_activity(&entry("w-stranded", WorkerLiveness::Running)),
            L::Idle,
            "a held slot with no turn in flight is incoherent state, not a worker awaiting input",
        );
    }

    /// `activity` is populated by the `workers__list` read path only; the
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
        let mut rx = workspace.subscribe().expect("subscribe");

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
    /// worker - which `workers__despawn` cannot clear - and the next boot
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
    /// `kind`, composed exactly as the spawn path composes them.
    fn forge_tool_surface(workspace: &Arc<Workspace>, kind: crate::mcp::SessionKind) -> String {
        let server = crate::mcp::build_forge_server(
            crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(workspace),
            crate::mcp::workers::facade::ProdWorkerFacade::from_arc(workspace),
            crate::mcp::review::facade::ProdReviewFacade::from_arc(workspace),
            crate::mcp::cron::facade::ProdCronFacade::from_arc(workspace),
            crate::mcp::gotify::facade::ProdGotifyFacade::from_arc(workspace),
            crate::mcp::slack::facade::ProdSlackFacade::from_arc(workspace),
            crate::mcp::tasks::facade::ProdTasksFacade::from_arc(workspace),
            SessionSlot::from_str_for_test("caller"),
            kind,
        );
        format!("{server:?}")
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
    /// reader to call `workers__spawn`, which is lead-only, so a worker
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
            preamble.contains("workers__spawn")
                && preamble.contains("never a peers call")
                && preamble.contains("Workers build; subagents review"),
            "a lead does get it",
        );
        assert!(
            !preamble.contains("doing the work yourself"),
            "the charter, not this block, carries the delegation default",
        );
    }

    /// The role a spawn carries is the role it gets, and the slot's label
    /// comes from the same value - so a worker cannot be handed a worker's
    /// tool surface AND the lead's address. A worker re-spawned by the
    /// boot resume path was classified as Lead while the key's shape was
    /// the only signal, which hands it the lead-only `peers__*` group; the
    /// caller that knows the row is a worker now says so.
    #[test]
    fn a_worker_spawn_carries_its_label_and_a_worker_tool_surface() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = seed_project_and_return(&ws, "forge", "/tmp/role-worker");
        let role =
            crate::protocol::SpawnRole::Worker { label: "implementer".to_owned(), wrote_row: true };

        let slot = Workspace::slot_for_spawn(&role, &project);
        assert_eq!(slot.label(), "implementer", "the slot names the worker");
        let kind = if slot.is_lead() {
            crate::mcp::SessionKind::Lead
        } else {
            crate::mcp::SessionKind::Worker
        };
        assert!(
            !forge_tool_surface(&ws, kind).contains("peers__"),
            "and a worker's forge server carries no peers tools",
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
    /// worker case while stripping peers from every lead.
    #[test]
    fn a_lead_tool_surface_keeps_its_peers_tools() {
        let (ws, _rx) = Workspace::testing_stub();
        assert!(
            forge_tool_surface(&ws, crate::mcp::SessionKind::Lead).contains("peers__ask_agent"),
            "a lead keeps its peers tools",
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
    /// it to one that was never meant to have it.
    #[test]
    fn dispatch_worker_respawns_carries_interactive_from_the_row() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.seed_test_project("proj-x", "/tmp/proj-x");
        let project_key = workspace.project_key_for_name("proj-x").expect("seeded project");
        let mut talkative = worker_row("talkative", None);
        talkative.interactive = Some(true);
        let dynamic = vec![talkative, worker_row("quiet", None)];

        workspace.dispatch_worker_respawns(
            &SessionSlot::from_str_for_test("new-lead"),
            &project_key,
            &dynamic,
            false,
        );

        for cmd in workspace.drain_test_dispatch_buffer() {
            let Command::SpawnWorker { label, interactive, from_boot_respawn, .. } = cmd else {
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
                }
                "quiet" => {
                    assert!(!interactive, "a non-interactive row re-spawns non-interactive");
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
        ws.record_worker_row(&key, "steward", session_id, "charter", None, None, false, false)
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
                )
                .await
        });
        let Command::SpawnWorker { resume_existing, return_to, .. } =
            take_dispatched_spawn_worker(&ws).await
        else {
            panic!("expected the resume to dispatch a SpawnWorker");
        };
        assert_eq!(
            resume_existing.as_deref(),
            Some(session_id),
            "the label resumes the session its transcript still names",
        );
        // Answer the way `handle_spawn_worker` does for a resume; what
        // the assertion below reads is the facade's own mapping of it,
        // which reports a fallback for a resume that found nothing.
        return_to
            .send(Ok(WorkerSpawnReply {
                session_id: session_id.to_owned(),
                tag: forge_primitives::worker_tag("steward"),
                rate_limited_account: None,
                durability_warning: None,
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
            .send(Ok(WorkerSpawnReply {
                session_id: "fresh-session-uuid".into(),
                tag: forge_primitives::worker_tag("ghost"),
                rate_limited_account: None,
                durability_warning: None,
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
            .send(Ok(WorkerSpawnReply {
                session_id: "fresh-session-uuid".into(),
                tag: forge_primitives::worker_tag("ghost"),
                rate_limited_account: None,
                durability_warning: None,
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
        let (workspace, _update_rx) = Workspace::testing_stub();
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
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
        assert_eq!(prompts.len(), 1, "exactly one WorkerSpawnFailedNotice envelope");
        if let Command::Prompt { key, text, .. } = prompts[0] {
            assert_eq!(*key, lead_key, "notice targets the lead session id");
            assert!(text.starts_with("[Worker 'reviewer' spawn failed"));
            assert!(text.contains("already used by worktree"));
        }

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
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
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

    /// handle_async_worker_spawn_failure expires worker-bound inflight
    /// asks: an ask buffered against a worker whose spawn dies was
    /// never delivered, so no target_session stamp exists and nothing
    /// else clears it - the caller would wait forever.
    #[tokio::test]
    async fn async_worker_spawn_failure_expires_worker_bound_asks() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk};
        let (workspace, _update_rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("proj-x");
        let worker_key = "builder-uuid";
        let session_key = SessionSlot::from_str_for_test(worker_key);
        workspace
            .insert_live_worker(&project_key, fake_worker("builder", worker_key, "lead", true));

        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Workers,
                caller: SessionSlot::from_str_for_test("lead-1"),
                target_project: crate::mcp::workers::worker_target_project_key("proj-x", "builder"),
                target_session: None,
            },
        );

        workspace.handle_async_worker_spawn_failure(
            &session_key,
            "resume failed: boom",
            SpawnFailureKind::Unclassified,
        );
        assert!(
            !workspace.inflight_asks.lock().contains_key(&id),
            "buffered worker ask expired on spawn failure"
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
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
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
    //! `Workspace::dispatch(Command::Prompt {..})` for each
    //! `KickRequest`, which the intercept buffer captures verbatim.
    //!
    //! Time is paused (`start_paused = true`) so the drainer's
    //! `tokio::time::sleep(KICK_DISPATCH_INTERVAL)` advances only when
    //! the test explicitly advances the clock. Without that, the
    //! drainer would race the assertions in real time.
    use super::*;
    use crate::protocol::Command;
    use std::time::Duration;

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
                Command::Prompt { key, .. } => Some(key),
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
        let prompts: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::Prompt { .. })).collect();
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
        let mut update_rx = workspace.subscribe().expect("single subscriber");
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
