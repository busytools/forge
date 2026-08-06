//! The orchestrator.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use forge_agent::AgentHandle;
use forge_agent::client::SessionLaunchSettings;
use forge_primitives::{PeerInflightStats, ReviewStatus, ReviewThread, SDKSessionInfo};

use crate::mcp::peers::types::{
    AskChannel, CorrelationId, InflightAsk, WrappedKind, WrappedPrompt,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::account::{self, AccountKey, AccountStateMap};
use crate::config::{LoadedConfig, LoadedProject, load_from_dir};
use crate::domain_session::DomainSession;
use crate::error::WorkspaceError;
use crate::protocol::{Command, DispatchError, SessionUpdate};
use crate::session_task::SessionTask;
use crate::spawn;
use crate::target::{ProjectKey, SessionKey, SessionTarget};
use crate::views::{ProjectView, SessionView};

/// How often the background poller refreshes account usage. The
/// TUI's bottom panel + the spawn-path account picker both read
/// from the cache this poll populates. 60 s upper-bounds how stale
/// the "which account has more headroom" decision can be while
/// staying clear of the OAuth usage endpoint's 429 throttle under
/// multi-instance polling - combined with per-account `last_error`
/// backoff (see `account::AccountState`), transient 429s recover
/// naturally.
const USAGE_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How often the cron scheduler wakes to fire due crons. Minute
/// granularity matches the cron-expression resolution.
const CRON_TICK_INTERVAL: Duration = Duration::from_secs(60);

/// A cron prompt buffered for an asleep owner: the raw prompt plus whether
/// this fire is overdue (delivered with a missed marker on drain).
#[derive(Debug)]
pub(crate) struct PendingCron {
    pub text: String,
    pub missed: bool,
}

/// Cron prompts buffered for asleep owners, keyed by `(project, team_role)`.
type PendingCronMap = HashMap<(String, Option<String>), Vec<PendingCron>>;

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

/// Forge-supplied resume kick for a dynamic worker on restart. Dynamic
/// (LLM-spawned) workers have no `resume-kick.md`, so on a resuming
/// re-spawn forge delivers this constant as the worker's first turn -
/// telling it to continue rather than start the task over.
const DYNAMIC_WORKER_RESTART_NOTE: &str = "This session was restarted by forge. Your prior conversation and progress are in the history above. Continue where you left off; do not restart the task.";

/// One enqueue onto the workspace-level worker-kick channel
/// (#259). Built by `maybe_kick_worker_on_connected` (and any
/// future kick site); drained by the workspace's
/// `start_kick_dispatcher` task, which fires one `Command::Prompt`
/// per `KICK_DISPATCH_INTERVAL` tick. Same payload shape as the
/// existing `Command::Prompt` carries (no attachments are ever
/// part of a kick prompt - kicks are pure text from
/// `<label>/kick.md` or `<label>/resume-kick.md`).
#[derive(Debug)]
pub(crate) struct KickRequest {
    pub session_key: SessionKey,
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
/// foreground colors + (for `Bailed`) a leading `⚠ ` glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChipState {
    /// Account is Ready and within budget. DIM foreground.
    Normal,
    /// Account is Ready but at least one usage window (5h or weekly)
    /// is currently at the cap. Yellow foreground signals "still
    /// spawns but expect throttling until the window resets." When
    /// every account is capped the plan is forced to assign one
    /// anyway, so every session's chip shows this state.
    AtCap,
    /// Account flipped to Bailed. Red foreground + `⚠ ` prefix.
    /// The session's spawn would fall through to round-robin until
    /// the recovery poll flips the account back to Ready.
    Bailed,
}

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
    config: LoadedConfig,
    /// Catalog of sessions per project. Seeded from
    /// `userdata::catalog::scan::list_sessions` at `new`; mutated
    /// in-place by [`Workspace::record_connected_session`] each time a
    /// freshly spawned session reaches `Connected`, so the Projects
    /// pane's drilldown stays current without forcing a full disk
    /// re-scan. Held under a Mutex because multiple in-process tasks
    /// (the pane render, the connect-flow event handler) reach for it
    /// across `await` points.
    catalog: Mutex<HashMap<ProjectKey, Vec<SDKSessionInfo>>>,
    /// Live Agents keyed by session id. `parking_lot::Mutex` so the
    /// public methods can take `&self`. `pub(crate)` so sibling
    /// modules (`spawn::handle_deliver_worker_prompt_to_lead`,
    /// `mcp::workers::facade::ProdWorkerFacade`) can probe pool
    /// membership for lead-delivery gating without an extra method
    /// wrapper.
    pub(crate) pool: Mutex<HashMap<SessionKey, PooledAgent>>,
    /// Account picker state. Updated on every spawn; refreshed by
    /// the in-memory usage poller.
    accounts: Mutex<AccountStateMap>,
    /// Deterministic per-session account assignment. `None` until the
    /// boot-time loading tasks reach `all_loaded()`; populated by
    /// `recompute_plan_if_ready`. Spawn paths consult this for
    /// CLAUDE_CONFIG_DIR selection; the launchpad gates clickable
    /// project rows on it being `Some` AND the project having a
    /// non-empty pool. See `crate::assignment_plan`.
    assignment_plan: Mutex<Option<crate::assignment_plan::AssignmentPlan>>,
    /// Fan-in [`SessionUpdate`] sender. Cloned and handed to TUI-side
    /// modules (slash executors, plugin install, service-status check)
    /// via [`Self::update_sender`] so they can emit presentation
    /// events on the same channel TUI subscribes to.
    update_tx: mpsc::UnboundedSender<SessionUpdate>,
    /// Single-take slot holding the matching receiver. [`Self::subscribe`]
    /// pops it on first call; subsequent calls return `None`.
    update_rx_slot: Mutex<Option<mpsc::UnboundedReceiver<SessionUpdate>>>,
    /// Per-session [`Command`] sender map. Populated when
    /// [`Self::get_agent_handle`] spawns the first `SessionTask` for a
    /// key; cleared on [`Self::release_session_with_cascade`] and [`Self::shutdown`].
    command_senders: Mutex<HashMap<SessionKey, mpsc::UnboundedSender<Command>>>,
    /// Per-project list of live worker sessions. In-memory only -
    /// wiped on forge restart by design (workers are ephemeral at the
    /// forge UI level; their JSONLs persist on disk). Mutated via
    /// `insert_live_worker` / `remove_latest_worker` / `drain_live_workers`.
    live_workers: Mutex<HashMap<ProjectKey, Vec<crate::mcp::workers::types::WorkerEntry>>>,
    /// Shared [`DomainSession`] handles, one per active `SessionTask`.
    /// `pub(crate)` so crate-internal spawn and delivery paths can
    /// reach a session's `DomainSession` directly.
    pub(crate) domain_handles: Mutex<HashMap<SessionKey, Arc<Mutex<DomainSession>>>>,
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
    pub(crate) peer_stats: Mutex<HashMap<SessionKey, PeerInflightStats>>,
    /// The session that submitted the reviews on a `(project, branch)` -
    /// the target for a worker's review-activity notice. Set by
    /// [`Self::submit_review`]; latest submit wins (the reviewer is one
    /// human whose session may rekey across a resume).
    review_origin: Mutex<HashMap<(String, String), SessionKey>>,
    /// Review actions a caller took during its current turn, keyed by the
    /// caller's [`SessionKey`]. Appended by [`Self::review_reply`] /
    /// [`Self::review_resolve`] and drained into one notice per review at
    /// the caller's turn end ([`Self::drain_review_activity`]).
    review_activity: Mutex<HashMap<SessionKey, Vec<crate::mcp::review::ReviewActivity>>>,
    /// Set the first time [`Self::start_usage_poller`] runs. Subsequent
    /// calls early-return to avoid spawning duplicate poller tasks.
    usage_poller_started: std::sync::atomic::AtomicBool,
    /// Guards against double-spawning the cron scheduler (mirrors
    /// `usage_poller_started`). Started once at boot from the binary.
    cron_scheduler_started: std::sync::atomic::AtomicBool,
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
    /// Wire-classification rewriter proxy started at workspace boot.
    /// Stamped onto every `Agent::spawn` so spawned subprocesses
    /// inherit `HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` and their wire
    /// classification gets normalised to `cli` (interactive
    /// subscription) shape. Forge refuses to construct a Workspace if
    /// the proxy fails to boot (hard-fail policy - see
    /// [`forge_agent::proxy`]).
    ///
    /// `None` only in `Workspace::testing_stub` - that path skips
    /// `new()` and therefore the proxy boot. Tests don't drive real
    /// subprocesses, so the absence is fine.
    proxy: Option<forge_agent::proxy::ProxyHandle>,
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
    crons: Mutex<Vec<forge_primitives::CronEntry>>,
    /// Cron prompts buffered for an asleep owner, keyed by
    /// `(project_name, team_role)` (`None` = lead). A due cron whose owner
    /// is asleep pushes here and dispatches `Command::SpawnProject`; the
    /// owner's session drains its own bucket on first `Connected`. One
    /// mechanism for lead and worker owners alike.
    pending_cron_by_owner: Mutex<PendingCronMap>,
    /// Active Gotify subscriptions (`mcp__forge__gotify`). The set the
    /// stream matches each inbound message against. Durable ones (lead /
    /// team-worker) are also persisted to `db` and reloaded here
    /// at boot; ephemeral ad-hoc-worker ones live only in memory.
    gotify_subs: Mutex<Vec<forge_primitives::GotifySubscription>>,
    /// Machine-local redb store. Backs durable Gotify subscriptions
    /// ([`crate::store::gotify`]) and persisted dynamic workers
    /// ([`crate::store::dynamic_workers`]). `None` when the DB couldn't
    /// open (degrade to in-memory-only, no persistence) or in
    /// `testing_stub`.
    db: Mutex<Option<crate::store::Db>>,
    /// Whether the Gotify stream is currently connected. Set by the
    /// subsystem pump on `Connected` / `Disconnected`; read by the
    /// Inspector's status line.
    gotify_connected: Mutex<bool>,
    /// Gotify application name -> numeric appid map, fetched from the
    /// server's `/application` list on subsystem start and refreshed on
    /// each reconnect. Resolves an `application` name filter to the appid
    /// inbound messages carry, and the reverse lookup for the envelope.
    gotify_app_index: Mutex<HashMap<String, u64>>,
    /// Shutdown handle for the running Gotify subsystem. `Some` while the
    /// stream task is live; dropping/sending stops it. `None` when idle
    /// (no subscriptions) or unconfigured. Guards against double-starting.
    gotify_subsystem: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Per-project in-flight guard for the engineering-team Connected
    /// hook's catalog scan. Inserted synchronously when
    /// `spawn_team_for_lead_with_catalog_scan` starts; removed when
    /// the async scan completes and dispatches its SpawnWorker
    /// commands. A concurrent second Connected (e.g. a fast /new
    /// reconnect) checking this set sees the entry and skips its own
    /// team-spawn, preventing duplicate worker sets while the scan
    /// is in flight. The existing `live_workers.is_empty()` gate
    /// covers the post-dispatch case.
    team_spawn_in_flight: Mutex<std::collections::HashSet<ProjectKey>>,
    /// Test-only intercept buffer for app-level Commands. When
    /// `Some`, `dispatch` captures the command into the buffer
    /// instead of routing it to the spawn::* handler - used by
    /// engineering-team tests to assert what would have been
    /// dispatched without spinning up real subprocesses. Always
    /// `None` in production (no enable hook outside test cfg).
    #[cfg(any(test, feature = "testing"))]
    command_intercept: Mutex<Option<Vec<Command>>>,
    /// Test-only project overlay. Entries appended via
    /// `seed_test_project_with_static_workers` are searched first in
    /// `find_project_view_by_name` so tests can drive the
    /// Connected-hook team-spawn trigger without writing a
    /// real `forge.toml`. Empty in production.
    #[cfg(any(test, feature = "testing"))]
    test_extra_projects: Mutex<Vec<LoadedProject>>,
}

/// Pool entry wrapping the live `Arc<AgentHandle>`. Tests assert
/// which account each spawn was bound to; that binding lives behind
/// `cfg(test)` so production carries no dead field.
pub(crate) struct PooledAgent {
    pub handle: Arc<AgentHandle>,
    #[cfg(test)]
    pub account: AccountKey,
}

/// Pick the lead session for a project from a list of candidates.
///
/// Order of preference:
/// 1. Latest by `last_modified` with `tag == forge:lead`.
/// 2. Latest by `last_modified` with no tag (lazy-migration fallback
///    so sessions that predate the workers feature stay reachable).
/// 3. `None` (caller spawns fresh).
///
/// `forge:worker:*`-tagged sessions are explicitly skipped: they are
/// not leads. Workers are typically already filtered upstream by
/// `list_sessions(..., include_workers = false)`, but this helper's
/// own filter ensures correctness if a caller passes
/// `include_workers = true`.
pub fn resolve_lead_session(sessions: &[SDKSessionInfo]) -> Option<&SDKSessionInfo> {
    let latest_with = |pred: fn(&&SDKSessionInfo) -> bool| -> Option<&SDKSessionInfo> {
        sessions.iter().filter(pred).max_by_key(|s| s.last_modified)
    };
    latest_with(|s| s.tag.as_deref() == Some(forge_primitives::FORGE_LEAD_TAG))
        .or_else(|| latest_with(|s| s.tag.is_none()))
}

/// Open the machine-local redb store at `<app_support>/db.redb`,
/// creating the app-support dir first. Returns `None` (with a warn) when
/// the dir can't be created or the DB can't open - forge then runs
/// without durable Gotify subscriptions or dynamic workers this session
/// (hard rule #15: no cwd fallback).
fn open_db(app_support: &Path) -> Option<crate::store::Db> {
    if let Err(error) = std::fs::create_dir_all(app_support) {
        tracing::warn!(
            target: "forge_workspace::workspace",
            %error,
            path = %app_support.display(),
            "creating the app-support dir failed; Gotify subscriptions will not persist",
        );
        return None;
    }
    match crate::store::Db::open(&app_support.join("db.redb")) {
        Ok(db) => Some(db),
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "opening the redb store failed; Gotify subscriptions will not persist",
            );
            None
        }
    }
}

/// The Gotify subsystem pump: run the reconnecting stream task and
/// translate its events into workspace state + message routing. Holds
/// only a `Weak<Workspace>` (upgraded per event) so it never keeps the
/// workspace alive; exits when `shutdown` fires or the stream task ends,
/// dropping its own handle to the stream task so that exits too.
async fn run_gotify_subsystem(
    weak: std::sync::Weak<Workspace>,
    cfg: forge_primitives::GotifyConfig,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    use forge_agent::env::gotify::GotifyEvent;

    let (tx, mut rx) = mpsc::channel::<GotifyEvent>(64);
    // Dropping this handle at fn-end signals the stream task to exit.
    let (_run_shutdown_tx, run_shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(forge_agent::env::gotify::run(cfg.clone(), tx, run_shutdown_rx));

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    GotifyEvent::Connected => {
                        // Refresh the app index off the pump so a slow (up to
                        // the 10s lookup timeout) or failing /application can't
                        // block shutdown or message routing.
                        tokio::spawn(refresh_gotify_app_index(weak.clone(), cfg.clone()));
                        if let Some(ws) = weak.upgrade() {
                            *ws.gotify_connected.lock() = true;
                        }
                    }
                    GotifyEvent::Disconnected => {
                        if let Some(ws) = weak.upgrade() {
                            *ws.gotify_connected.lock() = false;
                        }
                    }
                    GotifyEvent::Message(msg) => {
                        if let Some(ws) = weak.upgrade() {
                            ws.route_gotify_message(&msg);
                        }
                    }
                }
            }
        }
    }

    if let Some(ws) = weak.upgrade() {
        *ws.gotify_connected.lock() = false;
    }
}

/// Fetch the Gotify `/application` list and store the name->appid map.
/// Warns (never silently drops) on failure - an unresolved index would
/// otherwise leave every application-name-filtered subscription silently
/// matching nothing while the stream still reports connected.
async fn refresh_gotify_app_index(
    weak: std::sync::Weak<Workspace>,
    cfg: forge_primitives::GotifyConfig,
) {
    match forge_agent::env::gotify::app_index(&cfg).await {
        Ok(index) => {
            if let Some(ws) = weak.upgrade() {
                *ws.gotify_app_index.lock() = index;
            }
        }
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "Gotify /application lookup failed; application-name filters will not match until the next reconnect",
            );
        }
    }
}

/// Scan the catalog for `forge:worker:<label>` tagged sessions whose
/// `cwd` equals the label's run dir under `project_dir` (the project's
/// filesystem root). Returns one entry per role label, keyed by label
/// and valued by session_id. Used by the engineering-team Connected
/// hook to decide which roles to resume vs spawn fresh on forge restart.
///
/// Why scan the whole catalog rather than just `project_dir`'s own
/// subdir: workers spawned with `--worktree=<label>` `chdir` into
/// `<project_dir>/.claude/worktrees/<label>/` and claude indexes
/// their JSONLs under a DIFFERENT `<config_dir>/projects/<subdir>/`
/// keyed by the worktree path, not the main repo. A `directory=Some`
/// scan only walks one subdir; a worker in a worktree lives in a
/// SIBLING subdir, missing the filter. Switch to `directory=None`
/// (walk every project subdir) and match each session's `cwd` against
/// its label's run dir (worktree for a git worker, project root for a
/// non-git worker) so the pick lands where the resume read path reads.
///
/// Workers from OTHER projects have cwds outside every run dir and
/// are filtered out. Untagged or `forge:lead`-tagged sessions are
/// filtered out by the tag-prefix check.
///
/// Scans every account's `config_dir`: team workers pick their account
/// from the assignment-plan rotation, so a prior worker session can
/// live under any account, not just the workspace's canonical dir.
async fn scan_worker_resume_map(
    config_dirs: &[PathBuf],
    project_dir: &std::path::Path,
) -> HashMap<String, String> {
    let mut sessions: Vec<SDKSessionInfo> = Vec::new();
    for config_dir in config_dirs {
        sessions.extend(
            forge_agent::userdata::catalog::scan::list_sessions(config_dir, None, None, 0, true)
                .await,
        );
    }
    let is_git_repo = forge_agent::env::worktree::is_git_repo(project_dir);
    build_resume_map_from_sessions(&sessions, project_dir, is_git_repo)
}

/// Pure-function inner of [`scan_worker_resume_map`] - pulls the
/// catalog scan out so the filtering logic can be unit-tested without
/// the async filesystem walk. Returns label -> session_id, picking the
/// most-recently-modified worker-tagged session whose STORAGE KEY (the
/// `projects/<KEY>/` dir it physically lives in) equals the label's run
/// dir encoded the CLI way: `<project_dir>/.claude/worktrees/<label>`
/// for a git worker, `project_dir` for a non-git worker.
///
/// Scoping by the storage folder rather than the head-read `cwd` keeps
/// the pick aligned with the resume read path (which reads the JSONL
/// from `project_key_for_directory(worker_tag_dir(...))`) by
/// construction, and is immune to a `cwd` row that the lite metadata
/// read of the transcript head didn't capture.
fn build_resume_map_from_sessions(
    sessions: &[SDKSessionInfo],
    project_dir: &std::path::Path,
    is_git_repo: bool,
) -> HashMap<String, String> {
    // Most-recently-modified session per label wins, including across
    // sessions merged from multiple account config_dirs (list_sessions
    // sorts within one account, not across the merge).
    let mut ordered: Vec<&SDKSessionInfo> = sessions.iter().collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
    let mut resume_map: HashMap<String, String> = HashMap::new();
    for info in ordered {
        let Some(tag) = info.tag.as_deref() else {
            continue;
        };
        let Some(label) = tag.strip_prefix(forge_primitives::FORGE_WORKER_TAG_PREFIX) else {
            continue;
        };
        let run_dir = crate::mcp::workers::types::worker_tag_dir(project_dir, label, is_git_repo);
        let run_key = forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            run_dir.to_string_lossy().as_ref(),
        ));
        if info.storage_key != run_key {
            continue;
        }
        resume_map.entry(label.to_owned()).or_insert_with(|| info.session_id.clone());
    }
    resume_map
}

impl Workspace {
    /// Builds a Workspace, runs the catalog scan, and loads
    /// `<config_dir>/forge.toml`. Errors if `forge.toml` is missing
    /// or malformed (e.g. no `[[orgs]]` entries, no
    /// `[[orgs.projects]]` entries, unknown account references). No
    /// Agents are spawned on success.
    pub async fn new(config_dir: PathBuf) -> Result<Self, WorkspaceError> {
        Self::new_impl(config_dir, None).await
    }

    /// Like [`Workspace::new`] but opens the redb store under a tempdir
    /// inside `config_dir` rather than the real machine `app_support_dir`,
    /// so tests never touch the user's durable store (#392).
    #[cfg(any(test, feature = "testing"))]
    pub async fn new_for_test(config_dir: PathBuf) -> Result<Self, WorkspaceError> {
        let app_support = config_dir.join("app-support");
        Self::new_impl(config_dir, Some(app_support)).await
    }

    /// Shared constructor body. `app_support` supplies the redb base dir;
    /// `None` resolves the real machine `app_support_dir` and degrades to
    /// no durable store on failure (hard rule #15: no cwd fallback).
    async fn new_impl(
        config_dir: PathBuf,
        app_support: Option<PathBuf>,
    ) -> Result<Self, WorkspaceError> {
        let mut config = load_from_dir(&config_dir)?;

        // Create forge's own config subfolder before anything writes into
        // it (the lock, the cron + state stores all live under it). Hard-
        // fail if it can't be created: forge can persist nothing without a
        // writable config dir, so degrading is pointless - mirror the
        // ProxyUnavailable hard-fail below.
        crate::config::ensure_forge_data_dir(&config_dir).map_err(|source| {
            WorkspaceError::DataDirUnavailable {
                path: crate::config::forge_data_dir(&config_dir),
                source,
            }
        })?;

        // Single-instance guard: forge runs one process per config dir.
        // Take it BEFORE the proxy boot so a doomed second instance
        // refuses without binding proxy ports. The held File is stored
        // on `Self` for the process lifetime; flock auto-releases on
        // exit/crash. A second forge on the same config dir hard-fails
        // here exactly like ProxyUnavailable does below.
        let single_instance_lock = match crate::single_instance::acquire(&config_dir) {
            Ok(lock) => lock,
            Err(crate::single_instance::AcquireError::AlreadyRunning { pid }) => {
                return Err(WorkspaceError::AlreadyRunning { pid });
            }
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

        // Open the machine-local redb store: durable crons + Gotify
        // subscriptions both live in it. A failure to resolve the app-
        // support dir or open the DB degrades to no durable state this run
        // (non-fatal, like the state cache) - no cwd fallback (hard rule #15).
        let db = match app_support {
            Some(dir) => open_db(&dir),
            None => match forge_sdk::app_support_dir() {
                Ok(dir) => open_db(&dir),
                Err(error) => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        %error,
                        "app-support dir unresolved; Gotify subscriptions will not persist",
                    );
                    None
                }
            },
        };

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

        // Boot the wire-classification rewriter proxy BEFORE any
        // session can spawn. Hard-fail policy: if the proxy can't
        // bind / load its CA / build the TLS context, forge refuses
        // to start. The wire shape every spawned `claude` broadcasts
        // is correctness-critical (Anthropic tier classification), so
        // there is no "best-effort" fallback that lets sessions land
        // on metered tier silently.
        let proxy = forge_agent::proxy::start()
            .await
            .map_err(|e| WorkspaceError::ProxyUnavailable { reason: e.to_string() })?;

        // Catalog scan reads against the workspace's canonical
        // `config_dir` (where forge.toml lives). Each spawn binds to
        // its own account `config_dir` separately; multi-account
        // catalog merge is a separate concern.
        let catalog_entries = forge_agent::userdata::catalog::scan::list_sessions(
            &config_dir,
            None, // every project in the catalog
            None, // no limit
            0,
            false, // hide worker-tagged sessions from default catalog
        )
        .await;

        // Group sessions by project key derived from each session's cwd.
        // Sessions without a cwd are skipped - they can't be associated
        // with a project view.
        let mut catalog: HashMap<ProjectKey, Vec<SDKSessionInfo>> = HashMap::new();
        for entry in catalog_entries {
            if let Some(cwd) = entry.cwd.as_deref() {
                let key = ProjectKey::new(
                    forge_agent::userdata::catalog::scan::project_key_for_directory(Some(cwd)),
                );
                catalog.entry(key).or_default().push(entry);
            }
        }
        // The catalog scan returns entries sorted by `last_modified`
        // descending; the per-project Vec inherits that ordering thanks
        // to push order being preserved.

        let mut accounts = AccountStateMap::new(&config.accounts);

        // Seed account usage from the machine-local store so the
        // launchpad picker has tier data immediately at cold boot.
        // Anthropic's /api/oauth/usage rate-limiter can stall the first
        // live probe for 30 s+; without seed data every account ties at
        // tier 0 (unknown-fresh) during that window. The 60 s background
        // poller refreshes these snapshots - the cache is purely "last
        // known value" seed.
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

        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let (kick_dispatcher_tx, kick_dispatcher_rx) = mpsc::unbounded_channel::<KickRequest>();
        Ok(Self {
            config_dir,
            config,
            catalog: Mutex::new(catalog),
            pool: Mutex::new(HashMap::new()),
            accounts: Mutex::new(accounts),
            assignment_plan: Mutex::new(None),
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
            proxy: Some(proxy),
            _single_instance_lock: single_instance_lock,
            crons: Mutex::new(crons),
            pending_cron_by_owner: Mutex::new(HashMap::new()),
            gotify_subs: Mutex::new(gotify_subs),
            db: Mutex::new(db),
            gotify_connected: Mutex::new(false),
            gotify_app_index: Mutex::new(HashMap::new()),
            gotify_subsystem: Mutex::new(None),
            team_spawn_in_flight: Mutex::new(std::collections::HashSet::new()),
            #[cfg(any(test, feature = "testing"))]
            command_intercept: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_extra_projects: Mutex::new(Vec::new()),
        })
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

    /// The `[gotify]` server connection from forge.toml, or `None`
    /// when the section is absent. `None` keeps the Gotify subsystem
    /// dormant and makes `gotify__subscribe` error. Read-only - forge
    /// never writes forge.toml.
    pub fn gotify_config(&self) -> Option<forge_primitives::GotifyConfig> {
        self.config.gotify.clone()
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
        let open_sessions: std::collections::HashSet<SessionKey> =
            self.pool.lock().keys().cloned().collect();

        // One catalog acquire for the whole walk - the loop body is
        // a HashMap lookup + bounded cloning over owned session info
        // and never re-enters the catalog, so holding the lock for
        // the duration is cheap. The prior per-iteration
        // acquire/drop showed up as the dominant hot spot in
        // `ui::render` under projects with ~14 entries.
        let catalog = self.catalog.lock();
        // Iterate config.projects, with the test-only overlay appended
        // so unit tests can seed projects via `seed_test_project_with_static_workers`
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
                        .map(|info| {
                            let session = SessionKey::from_session_id(info.session_id.clone());
                            SessionView {
                                session: session.clone(),
                                label: info.summary.clone(),
                                is_open: open_sessions.contains(&session),
                                last_activity: Some(
                                    UNIX_EPOCH + Duration::from_millis(info.last_modified),
                                ),
                            }
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
                static_workers: project.static_workers.clone(),
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

    /// Build the rendered delegation catalog for a Lead session in the
    /// project named `namespace`. Always returns text (the capability
    /// preamble at minimum).
    fn build_delegation_catalog(namespace: &str) -> String {
        crate::team::catalog::render_catalog(&crate::team::catalog::scan_catalog(Some(namespace)))
    }

    /// Hands out the `Arc<AgentHandle>` for the requested session,
    /// spawning the underlying Agent lazily if it isn't already pooled.
    /// Idempotent - repeated calls for the same target return the same
    /// handle (no second subprocess). `settings` only apply to the
    /// spawn; subsequent calls reuse the existing Agent and ignore the
    /// parameter.
    ///
    /// Each fresh spawn consults the account picker and exports
    /// `CLAUDE_CONFIG_DIR` to the spawned `claude` subprocess so it
    /// reads/writes the picked account's config dir. Picker state
    /// lives in the in-memory usage cache; nothing about account
    /// choice is persisted across forge launches.
    ///
    /// Workspace does not track which handle the caller is "using" -
    /// that's the caller's concern.
    pub fn get_agent_handle(
        self: &Arc<Self>,
        target: SessionTarget,
        settings: SessionLaunchSettings,
    ) -> Result<Arc<AgentHandle>> {
        self.get_agent_handle_with_spawn_key(target, settings, None, None)
    }

    /// Like [`Self::get_agent_handle`] but threads a synthetic
    /// `spawn_key` onto the spawned `SessionTask`. The first
    /// `AgentEvent::Connected` arriving on the task drives a
    /// `SessionUpdate::KeyRenamed { from: spawn_key, to: real_key }`
    /// emit before the matching `Connected` so TUI re-keys its
    /// `UiSession` map atomically. `None` for re-entrant callers (the
    /// pooled handle path) where no key migration is needed.
    ///
    /// `forced_account` pins the spawn to a specific `(AccountKey,
    /// config_dir)` instead of running the assignment-plan /
    /// round-robin picker. Only the `/account` switch supplies it (via
    /// `handle_switch_account`); a forced account always re-spawns a
    /// live session, so the new `SessionTask` seeds `connected_once =
    /// true` and its first `Connected` emits `SessionReplaced` (the
    /// agent IS being replaced). Every other caller passes `None`.
    pub(crate) fn get_agent_handle_with_spawn_key(
        self: &Arc<Self>,
        target: SessionTarget,
        mut settings: SessionLaunchSettings,
        spawn_key: Option<SessionKey>,
        forced_account: Option<(AccountKey, PathBuf)>,
    ) -> Result<Arc<AgentHandle>> {
        let is_account_switch = forced_account.is_some();
        let session_key = self.resolve_target(&target)?;

        // Fast path: cache hit. When `spawn_key` was provided AND a
        // DomainSession is buffered there (peer-coordination path:
        // `handle_deliver_peer_prompt` parks the wrapped prompt at
        // `__spawn_<name>__` and dispatches SpawnProject), we MUST
        // drain that buffer into the live session before returning
        // the pooled handle - otherwise the pending peer prompt
        // strands at the synth key forever. The drain happens in
        // the same critical section as the pool lookup so a
        // concurrent caller can't race us into orphaning the
        // buffer.
        {
            let pool = self.pool.lock();
            if let Some(existing) = pool.get(&session_key) {
                let handle = Arc::clone(&existing.handle);
                drop(pool);
                self.drain_spawn_key_buffer_into(&session_key, spawn_key.as_ref());
                return Ok(handle);
            }
        }

        // Resolve which account this spawn lands under. Two paths:
        //
        // 1. AssignmentPlan lookup (preferred, deterministic): when
        //    the boot-time loading tasks have populated the plan
        //    AND we can derive (project_key, session_label) from
        //    target + spawn_key, look up the assignment directly.
        //    Lead sessions use label = "lead"; worker sessions look
        //    up their WorkerEntry in `live_workers` (which
        //    `handle_spawn_worker` inserted before reaching here)
        //    and read its label.
        //
        // 2. Round-robin fallback: pick_for_project. Used when the
        //    plan isn't populated yet (boot-time spawn before
        //    all_loaded), the label isn't in the plan (an adhoc
        //    worker spawned before `extend_plan_for_adhoc_worker`
        //    fires), or the target couldn't be resolved to a known
        //    project. Preserves the pre-#246 behaviour for cold-
        //    boot and unforeseen paths.
        let (account_key, account_dir) = forced_account.unwrap_or_else(|| {
            self.plan_assignment(&target, spawn_key.as_ref()).unwrap_or_else(|| {
                let project_account_pin = self.project_accounts_for(&target);
                let accounts = self.accounts.lock();
                accounts.pick_for_project(&project_account_pin)
            })
        });

        // Slow path: spawn fresh Agent bound to the picked account's
        // config_dir. The Agent stores it as a typed field; every
        // in-process accessor (oauth, settings, catalog scans) reads
        // it from there, and the spawned `claude` subprocess
        // inherits it as `CLAUDE_CONFIG_DIR` so each session reads/
        // writes the right account's user-data tree.
        // Attach the rewriter proxy only when the picked account's
        // `proxy = true` in forge.toml (defaults to true when the
        // field is absent). When false, claude talks direct to
        // Anthropic with native sdk-cli classification - used for
        // API-key accounts where the rewriter adds no value, or for
        // debugging the raw wire shape.
        let (account_proxy_enabled, account_env) = {
            let accounts = self.accounts.lock();
            (
                accounts.proxy_enabled(&account_key),
                accounts.env(&account_key).cloned().unwrap_or_default(),
            )
        };
        let session_env = self.session_env_for(&target, &account_env);
        let attached_proxy = if account_proxy_enabled { self.proxy.clone() } else { None };

        // Hoist DomainSession creation to BEFORE Agent::spawn so the
        // per-session peer-MCP server's CallerKeyResolver can read
        // back through the same Arc<Mutex<DomainSession>>. The
        // existing connect::create_app path may have registered a
        // pre-Connect placeholder under `session_key` (conn = None);
        // reuse it when present so the TUI's pre-spawn accessors
        // keep their handle reference. Otherwise create fresh with
        // conn = None and update post-Agent::spawn at line ~470.
        let domain_arc = {
            let mut handles = self.domain_handles.lock();
            // Three cases:
            //  1. A DomainSession is already registered at `session_key`
            //     (e.g. connect::create_app's pre-Connect placeholder).
            //     Reuse it.
            //  2. `spawn_key` was provided AND a DomainSession exists
            //     there (peer-coordination spawn path: handle_deliver_
            //     peer_prompt pre-populated pending_peer_prompts at
            //     synth_key=`__spawn_<name>__` before dispatching
            //     SpawnProject). Move that
            //     DomainSession onto `session_key` so the SessionTask
            //     we're about to construct sees the buffered state.
            //  3. Neither - create fresh at `session_key`.
            //
            // When both `session_key` and `spawn_key` exist (race:
            // peer ask arrives while a pre-Connect placeholder was
            // already there), merge `spawn_key`'s buffered prompts
            // into the placeholder. The placeholder is the
            // one the SessionTask will pick up via `session_key`.
            if let Some(existing) = handles.get(&session_key).cloned() {
                if let Some(spawn) = spawn_key.as_ref()
                    && spawn != &session_key
                    && let Some(buffered) = handles.remove(spawn)
                {
                    Self::merge_spawn_buffer_into_placeholder(&existing, &buffered);
                }
                existing
            } else if let Some(spawn) = spawn_key.as_ref()
                && spawn != &session_key
                && let Some(buffered) = handles.remove(spawn)
            {
                buffered.lock().key = session_key.clone();
                handles.insert(session_key.clone(), Arc::clone(&buffered));
                buffered
            } else {
                let fresh = Arc::new(Mutex::new(DomainSession::new(session_key.clone(), None)));
                handles.insert(session_key.clone(), Arc::clone(&fresh));
                fresh
            }
        };
        // Carry `--new` onto the domain so a project lead's
        // Connected-time team spawn skips the worker resume scan and
        // brings its workers up fresh alongside the fresh lead.
        domain_arc.lock().spawned_force_new = settings.force_new;

        // Build the per-session `forge` MCP server. ONE server name;
        // tool surface depends on whether this spawn is for a project
        // lead or a worker. Leads see peers + workers (cross-project
        // coordination is a lead-only role); workers see workers
        // only. See `crate::mcp::SessionKind` for the rationale.
        //
        // The synth-key prefix is the signal: `__spawn_worker_*` is
        // stamped by `handle_spawn_worker` for every worker spawn;
        // peer-spawned project leads use `__spawn_<name>__` (no
        // `worker` segment) and direct lead spawns have no
        // `spawn_key`. So absence of the worker prefix means Lead.
        let session_kind =
            if spawn_key.as_ref().is_some_and(|k| k.as_str().starts_with("__spawn_worker_")) {
                crate::mcp::SessionKind::Worker
            } else {
                crate::mcp::SessionKind::Lead
            };
        let forge_server = {
            let workspace_facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(self);
            let worker_facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(self);
            let review_facade = crate::mcp::review::facade::ProdReviewFacade::from_arc(self);
            let cron_facade = crate::mcp::cron::facade::ProdCronFacade::from_arc(self);
            let gotify_facade = crate::mcp::gotify::facade::ProdGotifyFacade::from_arc(self);
            let resolver = crate::mcp::peers::facade::CallerKeyResolver::from_domain(&domain_arc);
            crate::mcp::build_forge_server(
                workspace_facade,
                worker_facade,
                review_facade,
                cron_facade,
                gotify_facade,
                resolver,
                session_kind,
            )
        };

        let handle = forge_agent::Agent::spawn(
            account_dir.clone(),
            Some(account_key.0.clone()),
            attached_proxy,
            vec![("forge".to_owned(), forge_server)],
            session_env,
        );
        // Project-rooted targets (`Default` / `Named`) resume the
        // project's lead session when the on-disk catalog has one,
        // and fall back to a fresh session in that project's cwd
        // otherwise. Pool key = lead's session id from the catalog
        // so it stays consistent with the running session id.
        match target {
            SessionTarget::Default => {
                let project = self.config.default_project();
                if matches!(session_kind, crate::mcp::SessionKind::Lead) {
                    settings.delegation_catalog =
                        Some(Self::build_delegation_catalog(&project.name));
                }
                let cwd = project.path.to_string_lossy().to_string();
                let resume_target = Self::apply_force_new_gate(
                    self.try_lead_session_id_for(project),
                    settings.force_new,
                );
                if let Some(lead) = resume_target {
                    handle.resume_or_new_session(lead.as_str().to_owned(), cwd, settings)?;
                } else {
                    handle.new_session(cwd, settings)?;
                }
            }
            SessionTarget::Named(name) => {
                let project = self.find_project_by_name(&name)?;
                if matches!(session_kind, crate::mcp::SessionKind::Lead) {
                    settings.delegation_catalog =
                        Some(Self::build_delegation_catalog(&project.name));
                }
                let cwd = project.path.to_string_lossy().to_string();
                let resume_target = Self::apply_force_new_gate(
                    self.try_lead_session_id_for(project),
                    settings.force_new,
                );
                if let Some(lead) = resume_target {
                    handle.resume_or_new_session(lead.as_str().to_owned(), cwd, settings)?;
                } else {
                    handle.new_session(cwd, settings)?;
                }
            }
            SessionTarget::Session(key) => {
                let cwd = self.resume_cwd_for_session(&key);
                handle.resume_session(key.as_str().to_owned(), cwd, settings)?;
            }
            SessionTarget::FreshInProject { project_key, .. } => {
                // Worker spawn: a fresh session in the project's cwd.
                // Skip the lead-resume path so each worker is a new
                // claude-issued session UUID (the lead lives untouched).
                let project = self
                    .config
                    .projects
                    .iter()
                    .find(|p| {
                        forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                            &p.path.to_string_lossy(),
                        )) == project_key.as_str()
                    })
                    .ok_or_else(|| WorkspaceError::ProjectNotFound {
                        name: project_key.as_str().to_owned(),
                        path: crate::config::forge_data_dir(&self.config_dir).join("forge.toml"),
                    })?;
                let cwd = project.path.to_string_lossy().to_string();
                handle.new_session(cwd, settings)?;
            }
        }

        let arc = Arc::new(handle);

        // Insert: race-safe via "if absent" semantics. If a concurrent
        // caller raced us to the spawn, theirs wins the pool slot and
        // ours drops at end-of-scope (subprocess killed via Client's
        // existing Drop). Single-user scope makes the race effectively
        // impossible.
        {
            let mut pool = self.pool.lock();
            if let Some(existing) = pool.get(&session_key) {
                return Ok(Arc::clone(&existing.handle));
            }
            pool.insert(
                session_key.clone(),
                PooledAgent {
                    handle: Arc::clone(&arc),
                    #[cfg(test)]
                    account: account_key,
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
            if senders.contains_key(&session_key) {
                None
            } else {
                let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
                senders.insert(session_key.clone(), cmd_tx);
                Some(cmd_rx)
            }
        };
        if let Some(cmd_rx) = cmd_rx {
            // `domain_arc` was hoisted above so the peer-MCP server's
            // CallerKeyResolver could reference it pre-Agent::spawn.
            // Stamp the live `Arc<AgentHandle>` onto its `conn` slot
            // now that the handle exists. The TUI's pre-spawn
            // accessors (`connect::create_app`'s placeholder entry)
            // keep reading from the same `Arc<Mutex<...>>` they were
            // given before - no second `domain_session_for`
            // round-trip after the spawn lands.
            domain_arc.lock().conn = Some(Arc::clone(&arc));
            let domain = Arc::clone(&domain_arc);
            let task = SessionTask {
                key: session_key,
                handle: Arc::clone(&arc),
                command_rx: cmd_rx,
                domain,
                update_tx: self.update_tx.clone(),
                spawn_key,
                // An account switch replaces a live session's agent, so
                // the new task's first Connected must emit SessionReplaced
                // (reset chat, then the --resume backfill re-seeds it).
                connected_once: is_account_switch,
                workspace: Arc::downgrade(self),
            };
            let span = tracing::info_span!(
                "session_task",
                key = %task.key.as_str(),
            );
            tokio::spawn(task.run().instrument(span));
        }

        Ok(arc)
    }

    /// When `get_agent_handle_with_spawn_key` hits the pool fast-path
    /// for a session that's already running, drain any
    /// `pending_peer_prompts` buffered at the synthetic `spawn_key` (e.g.
    /// `__spawn_<project>__`) into the live session via `Command::Prompt`.
    /// Without this, peer asks aimed at a running-but-pre-spawn-dispatched
    /// target strand at the synth key forever - the regular Connected-time
    /// drain only fires when a fresh SessionTask boots. Cron prompts use
    /// the owner-keyed buffer, not the synth key, so they are not drained
    /// here.
    fn drain_spawn_key_buffer_into(
        self: &Arc<Self>,
        session_key: &SessionKey,
        spawn_key: Option<&SessionKey>,
    ) {
        let Some(spawn_key) = spawn_key else { return };
        if spawn_key == session_key {
            return;
        }
        let buffered_domain = self.domain_handles.lock().remove(spawn_key);
        let Some(buffered_domain) = buffered_domain else { return };
        let pending = {
            let mut guard = buffered_domain.lock();
            std::mem::take(&mut guard.pending_peer_prompts)
        };
        if pending.is_empty() {
            return;
        }
        // Re-dispatch each buffered prompt against the live session.
        // Mirrors session_task::drain_pending_peer_prompts: bump
        // IncomingPlus1 only for Question wrappers, push a
        // synthetic user-turn so the TUI's chat shows the inbound
        // block (claude CLI doesn't echo stdin-injected prompts),
        // then dispatch the Command::Prompt.
        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(self);
        for wrapped in pending {
            if matches!(wrapped.kind, WrappedKind::Question) {
                facade.bump_inflight_stats(
                    session_key,
                    crate::mcp::peers::facade::PeerStatsDelta::IncomingPlus1,
                );
                self.stamp_inflight_target(&wrapped.correlation_id, session_key);
            }
            crate::spawn::push_peer_user_turn_into_chat(self, session_key, &wrapped);
            let text = wrapped.to_prose();
            if let Err(err) = self.dispatch(crate::protocol::Command::Prompt {
                key: session_key.clone(),
                text,
                attachments: Vec::new(),
            }) {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    key = %session_key.as_str(),
                    error = ?err,
                    "drain_spawn_key_buffer_into: dispatch failed; prompt dropped",
                );
            }
        }
    }

    /// Merge a synthetic spawn-key DomainSession's buffered peer prompts
    /// into an existing placeholder (Case 1 in
    /// `get_agent_handle_with_spawn_key`: a peer ask arrived while a
    /// pre-Connect placeholder already sat at `session_key`). Cron prompts
    /// live in the owner-keyed buffer, not on the synth key, so they need
    /// no merge here.
    fn merge_spawn_buffer_into_placeholder(
        placeholder: &Mutex<DomainSession>,
        buffered: &Mutex<DomainSession>,
    ) {
        let mut placeholder = placeholder.lock();
        let mut src = buffered.lock();
        placeholder.pending_peer_prompts.append(&mut src.pending_peer_prompts);
    }

    /// Crate-internal accessor for the boot-time loading task to
    /// drive `LoadingState` transitions on the workspace's account
    /// map. Not exposed beyond the crate - the field stays private
    /// so only intentional callers reach in.
    pub(crate) fn account_states(&self) -> &Mutex<AccountStateMap> {
        &self.accounts
    }

    /// `true` when every `[[accounts]]` entry has reached a terminal
    /// `LoadingState`. The launchpad reads this to decide whether
    /// project rows are clickable - clicking before all accounts
    /// resolve means the spawn-time picker falls back to round-robin
    /// (with potentially-undesirable account choice) instead of the
    /// deterministic plan. Public so forge-tui can gate keyboard +
    /// mouse handlers on it without reaching into the crate-private
    /// account map.
    pub fn all_accounts_loaded(&self) -> bool {
        self.accounts.lock().all_loaded()
    }

    /// `true` when the assignment plan is populated AND has at least
    /// one entry for `project_key`. Projects whose pool resolved to
    /// empty at compute time (e.g., every allowed account Bailed)
    /// produce zero entries; the launchpad surfaces a
    /// `no usable accounts` hint for those rows and keeps them
    /// unclickable. Returns `false` when the plan isn't populated yet.
    /// Public for forge-tui to consult during render.
    pub fn project_has_assigned_account(&self, project_key: &ProjectKey) -> bool {
        let plan = self.assignment_plan.lock();
        plan.as_ref().is_some_and(|p| !p.project_has_no_assignments(project_key))
    }

    /// Snapshot of `(AccountKey display name, LoadingState)` pairs in
    /// declaration order. Forge-tui's launchpad renders the per-
    /// account loading glyph row from this; the order matches
    /// `forge.toml`'s `[[accounts]]` declarations so the glyphs sit
    /// next to the user's mental model of which-account-is-which.
    pub fn account_loading_snapshot(&self) -> Vec<(String, crate::account::LoadingState)> {
        let accounts = self.accounts.lock();
        accounts.ordered_keys.iter().map(|k| (k.0.clone(), accounts.loading_state(k))).collect()
    }

    /// Resolve `(project, session_label)` to the per-session chip
    /// the Projects pane renders next to each row: the assigned
    /// account name + its current visual-state category. Returns
    /// `None` when the plan isn't populated, the project isn't
    /// known, or the label has no assignment.
    ///
    /// State derivation:
    /// - `LoadingState::Bailed` -> `SessionChipState::Bailed`
    ///   (renderer renders red with a `⚠ ` prefix).
    /// - `Ready` + any usage window at the cap (the same
    ///   `is_saturated` signal the assignment plan uses to avoid an
    ///   account) -> `AtCap` (yellow; the session still spawns but
    ///   will throttle - and in the all-accounts-capped case it was
    ///   assigned only because nothing else was free).
    /// - Otherwise -> `Normal` (DIM; default chip).
    pub fn session_chip_for(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) -> Option<SessionChipInfo> {
        let plan_guard = self.assignment_plan.lock();
        let plan = plan_guard.as_ref()?;
        let account_key = plan.lookup(project_key, &label.to_owned())?.clone();
        drop(plan_guard);

        let accounts = self.accounts.lock();
        let loading = accounts.loading_state(&account_key);
        let saturated = accounts.is_saturated(&account_key);
        drop(accounts);

        let state = match loading {
            crate::account::LoadingState::Bailed => SessionChipState::Bailed,
            crate::account::LoadingState::Ready if saturated => SessionChipState::AtCap,
            _ => SessionChipState::Normal,
        };

        Some(SessionChipInfo { account_name: account_key.0, state })
    }

    /// Look up the deterministic account assignment for a spawn
    /// target. Returns `(AccountKey, config_dir)` when:
    /// - the assignment plan is populated (`Some`), AND
    /// - the spawn resolves to a known (project_key, session_label)
    ///   pair, AND
    /// - the plan has an entry for that pair.
    ///
    /// Returns `None` for any miss so the caller falls back to the
    /// pre-#246 round-robin (`pick_for_project`). This keeps boot-
    /// time spawns (plan not yet populated) working and absorbs
    /// edge cases like adhoc workers spawned before
    /// `extend_plan_for_adhoc_worker` runs.
    pub(crate) fn plan_assignment(
        &self,
        target: &SessionTarget,
        spawn_key: Option<&SessionKey>,
    ) -> Option<(AccountKey, std::path::PathBuf)> {
        let (project_key, label) = self.plan_lookup_keys(target, spawn_key)?;
        let plan_guard = self.assignment_plan.lock();
        let plan = plan_guard.as_ref()?;
        let account_key = plan.lookup(&project_key, &label)?.clone();
        drop(plan_guard);
        let accounts = self.accounts.lock();
        let dir = accounts.config_dir(&account_key)?.clone();
        Some((account_key, dir))
    }

    /// Derive `(project_key, session_label)` from a spawn target +
    /// optional synth spawn key.
    ///
    /// - Worker spawns (`spawn_key` points at an existing
    ///   `WorkerEntry` in `live_workers`): the entry's
    ///   `(project_key, label)` is the answer. This is the path
    ///   `handle_spawn_worker` -> `get_agent_handle_with_spawn_key`
    ///   takes after inserting the WorkerEntry as `Spawning`.
    /// - Lead spawns (no spawn_key or a non-worker synth key):
    ///   `label = "lead"`; `project_key` derives from the target.
    /// - Session-id targets where the resumed session has no
    ///   recoverable project mapping: returns `None` so the caller
    ///   falls back to round-robin.
    fn plan_lookup_keys(
        &self,
        target: &SessionTarget,
        spawn_key: Option<&SessionKey>,
    ) -> Option<(ProjectKey, String)> {
        if let Some(key) = spawn_key
            && let Some(pair) = self.worker_label_for_spawn_key(key)
        {
            return Some(pair);
        }
        let project_key = self.target_to_project_key(target)?;
        Some((project_key, "lead".to_owned()))
    }

    /// Walk `live_workers` looking for an entry whose `session_key`
    /// matches `spawn_key`. The map has bounded size in practice
    /// (per-project worker counts are small), so an O(N*M) walk is
    /// fine - the alternative would be a second index keyed by
    /// session_key, which adds maintenance burden for marginal gain.
    fn worker_label_for_spawn_key(&self, spawn_key: &SessionKey) -> Option<(ProjectKey, String)> {
        let workers = self.live_workers.lock();
        for (project_key, entries) in workers.iter() {
            for entry in entries {
                if &entry.session_key == spawn_key {
                    return Some((project_key.clone(), entry.label.clone()));
                }
            }
        }
        None
    }

    /// Resolve a `SessionTarget` to its on-disk-sanitised project
    /// key. Returns `None` for `Session(...)` targets when the
    /// catalog can't map the session's cwd back to a known project
    /// (e.g., a session whose cwd has since changed).
    fn target_to_project_key(&self, target: &SessionTarget) -> Option<ProjectKey> {
        let project_key_for = |path: &std::path::Path| -> ProjectKey {
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                &path.to_string_lossy(),
            )))
        };
        match target {
            SessionTarget::Default => Some(project_key_for(&self.config.default_project().path)),
            SessionTarget::Named(name) => {
                self.find_project_by_name(name).ok().map(|p| project_key_for(&p.path))
            }
            SessionTarget::FreshInProject { project_key, .. } => Some(project_key.clone()),
            SessionTarget::Session(key) => {
                let cwd = self.session_cwd_for(key)?;
                let cwd_path = std::path::PathBuf::from(&cwd);
                // A worktree cwd misses this exact match and yields None,
                // degrading account selection to the worktree-aware
                // project_accounts_for fallback. Routing resumed worktree
                // workers through the plan here is a separate follow-up.
                self.config
                    .projects
                    .iter()
                    .find(|p| p.path == cwd_path)
                    .map(|p| project_key_for(&p.path))
            }
        }
    }

    /// Extend the assignment plan with a new adhoc worker. Called
    /// from `handle_spawn_worker` (Section 2.5 of #246) so workers
    /// spawned via `workers__spawn` are assigned through the same
    /// plan-driven rotation as boot-time team members. No-op when
    /// the plan isn't populated yet (boot still in flight) - the
    /// fallback `pick_for_project` path takes over in that case.
    /// Returns `Some(account)` when the assigned account is itself
    /// currently unusable (a fresh assignment that fell back onto a
    /// fully saturated pool, or a re-spawn pinned to a since-unusable
    /// account), for the caller to surface at spawn; `None` on a usable
    /// assignment or a no-op.
    pub(crate) fn extend_plan_for_adhoc_worker(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) -> Option<AccountKey> {
        // Snapshot per-account usability under the accounts lock, then
        // release it before taking the plan lock: no path holds both
        // locks at once (each site drops the first guard before taking
        // the second), so the two orders can't form a cycle. The
        // snapshot doubles as the rotation predicate and the
        // warn-surface check below.
        let usable: std::collections::HashSet<AccountKey> = {
            let accounts = self.accounts.lock();
            accounts
                .ordered_keys
                .iter()
                // Mirror the ready_accounts + pick_for_project experimental
                // filters. Defensive: the plan pool never holds an
                // experimental account, so this only keeps the three
                // assignment-path predicates consistent.
                .filter(|k| accounts.is_account_usable(k) && !accounts.is_experimental(k))
                .cloned()
                .collect()
        };

        let assigned = {
            let mut plan_guard = self.assignment_plan.lock();
            let plan = plan_guard.as_mut()?;
            plan.assign_adhoc_worker(project_key, &label.to_owned(), |k| usable.contains(k))
        }?;

        // The assigned account is currently unusable: either the fresh
        // rotation found the whole pool saturated, or a re-spawn is
        // pinned to an account that has since gone unusable. Warn and
        // return it so the caller surfaces the state at spawn.
        if usable.contains(&assigned) {
            return None;
        }
        tracing::warn!(
            target: "forge_workspace::account",
            label,
            account = %assigned.0,
            "adhoc worker assigned to a rate-limited or bailed account",
        );
        Some(assigned)
    }

    /// Recompute the `AssignmentPlan` from the current ready-account
    /// set when every account has reached a terminal `LoadingState`.
    /// No-op when accounts are still loading - the boot-time
    /// `account_loader` task re-calls this after each state
    /// transition, so the plan ends up populated on the first
    /// `all_loaded`-true call. Subsequent transitions (e.g., a
    /// runtime 401 flipping a Ready account to Bailed) also trigger
    /// a recompute via the same path; Section 4.4 of #246 swaps
    /// this for a frozen-overlay variant that preserves existing
    /// assignments while extending the plan with newly-recovered
    /// accounts.
    pub(crate) fn recompute_plan_if_ready(&self) {
        use crate::account::LoadingState;
        use crate::assignment_plan::{ProjectInput, compute_plan};

        let (ready_accounts, saturated): (Vec<AccountKey>, Vec<AccountKey>) = {
            let accounts = self.accounts.lock();
            if !accounts.all_loaded() {
                return;
            }
            // Iterate in forge.toml definition order, not HashMap order:
            // compute_plan is documented pure, and for a project with an
            // empty `accounts` list the pool IS this slice, so HashMap
            // randomness would assign the lead to a different account
            // across restarts.
            let ready: Vec<AccountKey> = accounts
                .ordered_keys
                .iter()
                // Experimental accounts never enter the assignment pool
                // (leads, static + adhoc workers) even when a project's
                // org pins them; they are reachable only via the
                // `/account` picker.
                .filter(|k| !accounts.is_experimental(k))
                .filter(|k| {
                    accounts
                        .by_key
                        .get(*k)
                        .is_some_and(|s| matches!(s.loading, LoadingState::Ready))
                })
                .cloned()
                .collect();
            // Accounts that loaded fine but sit at the usage cap. The
            // plan prefers the rest so a freshly-exhausted account
            // doesn't get sessions assigned to it on boot.
            let saturated: Vec<AccountKey> =
                ready.iter().filter(|k| accounts.is_saturated(k)).cloned().collect();
            (ready, saturated)
        };

        let projects: Vec<ProjectInput> = self
            .config
            .projects
            .iter()
            .map(|p| ProjectInput {
                key: ProjectKey::new(
                    forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                        &p.path.to_string_lossy(),
                    )),
                ),
                accounts: p.accounts.clone(),
                static_workers: p.static_workers.clone(),
            })
            .collect();

        let fresh = compute_plan(&ready_accounts, &saturated, &projects);
        let mut plan_guard = self.assignment_plan.lock();
        match plan_guard.as_mut() {
            // First compute - just store. Boot-time path.
            None => *plan_guard = Some(fresh),
            // Subsequent recompute (e.g., Bailed account recovered).
            // Frozen overlay preserves existing (project, label)
            // assignments so mid-run sessions don't shift to a
            // newly-recovered account; extends the plan with
            // newly-recovered accounts for future spawns.
            Some(existing) => existing.merge_frozen(fresh),
        }
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
        let entries: Vec<(AccountKey, std::path::PathBuf)> = {
            let accounts = self.accounts.lock();
            accounts
                .ordered_keys
                .iter()
                .filter_map(|key| accounts.config_dir(key).map(|dir| (key.clone(), dir.clone())))
                .collect()
        };
        for (key, dir) in entries {
            let span = tracing::info_span!("account_loading", account = %key.0);
            let weak = Arc::downgrade(self);
            tokio::spawn(
                async move {
                    crate::account_loader::run_account_loading(dir, key, weak).await;
                }
                .instrument(span),
            );
        }

        // Background recovery poll: watches Bailed accounts and
        // re-runs the loading flow when `claude auth status` flips
        // back to logged-in. One task per Workspace lifetime. Holds
        // Weak so the task auto-exits on workspace shutdown rather
        // than keeping the Arc alive past drop.
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("account_recovery_poll");
        tokio::spawn(
            async move {
                crate::account_loader::run_recovery_poll(weak).await;
            }
            .instrument(span),
        );
    }

    /// Spawn the 60 s background account-usage poller. Fetches
    /// OAuth usage for every `[[accounts]]` entry via the per-
    /// account config-dir's credentials file (no Agent spawn
    /// required), writes each result into `AccountStateMap.by_key`.
    /// The TUI's bottom panel + the spawn-path picker both read
    /// from that cache.
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
                    let session_key = req.session_key.clone();
                    if let Err(err) = workspace.dispatch(crate::protocol::Command::Prompt {
                        key: req.session_key,
                        text: req.prompt_body,
                        attachments: Vec::new(),
                    }) {
                        tracing::error!(
                            target: "forge_workspace::workspace",
                            key = %session_key.as_str(),
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
        let entries: Vec<(
            AccountKey,
            std::path::PathBuf,
            std::collections::HashMap<String, String>,
        )> = {
            let accounts = self.accounts.lock();
            accounts
                .ordered_keys
                .iter()
                // Skip accounts inside an active backoff window - a
                // recent probe failed and re-probing now would just
                // re-trip the same rate limit. `scheduler_should_probe`
                // ORs the backoff gate (`should_probe_now`) with the
                // one-shot reset-clear override hook
                // (`has_just_cleared_cap_window`): cold-cache accounts
                // probe immediately, and a snapshot whose resets_at
                // moment has passed gets a fresh probe via the
                // override even if the backoff timer is still active.
                .filter(|key| accounts.scheduler_should_probe(key))
                .filter_map(|key| {
                    accounts.config_dir(key).map(|dir| {
                        (key.clone(), dir.clone(), accounts.env(key).cloned().unwrap_or_default())
                    })
                })
                .collect()
        };

        // Disarm any account where the override hook was the deciding
        // factor (should_probe_now was false but the hook said yes).
        // One-shot semantics: each successful probe arms the hook once
        // via set_usage; the override fires once, and subsequent
        // stale-reset state respects the backoff schedule until a
        // fresh snapshot lands. Without this disarm, a persistently
        // failing probe series would re-trigger the override every
        // poll cycle, defeating the exponential backoff. Held in its
        // own lock acquisition so it doesn't contend with the
        // per-iteration set_usage / set_last_error locks below.
        {
            let mut accounts = self.accounts.lock();
            for (key, _, _) in &entries {
                if !accounts.should_probe_now(key) {
                    accounts.disarm_override(key);
                }
            }
        }
        // Sequential probes. Anthropic's `/api/oauth/usage` endpoint
        // has a per-IP burst limit; parallel spawns trip the limit
        // and produce HTTP 429s even well under the user's own quota.
        // Serial execution staggers requests by per-probe latency
        // (~hundreds of ms), within the 60 s poll interval.
        let mut any_success = false;
        for (key, dir, env) in entries {
            // One decision (probe_plan) drives both the probe source and
            // the response-mapping strictness. A base-url-override account
            // probes its own endpoint with the env bearer (skipping the
            // keychain + the auth-refresh wrapper) and maps leniently
            // (each window independent; cold `{}` -> n/a); a normal
            // account keeps the keychain + default host + strict mapping.
            let plan = forge_agent::cloud::oauth_usage::probe_plan(&env);
            let is_base_url =
                matches!(plan, forge_agent::cloud::oauth_usage::ProbePlan::BaseUrl { .. });
            let fetch_result = match &plan {
                forge_agent::cloud::oauth_usage::ProbePlan::BaseUrl { base_url, bearer } => {
                    let creds = forge_agent::cloud::oauth_credentials::OauthCredentials {
                        access_token: bearer.clone(),
                        expires_at: None,
                    };
                    forge_agent::cloud::oauth_usage::probe(&creds, Some(base_url)).await
                }
                forge_agent::cloud::oauth_usage::ProbePlan::Keychain => {
                    forge_agent::cloud::oauth_usage::oauth_usage(&dir).await
                }
            };
            match fetch_result {
                Ok(payload) => {
                    match forge_agent::cloud::oauth::map_probe_snapshot(is_base_url, payload) {
                        Ok(snapshot) => {
                            self.accounts.lock().set_usage(&key, snapshot);
                            any_success = true;
                        }
                        Err(err) => {
                            self.accounts.lock().set_last_error(
                                &key,
                                crate::account::UsageFetchStatus::Other,
                                None,
                            );
                            tracing::debug!(
                                target: "forge_workspace::account",
                                account = %key.0,
                                error = ?err,
                                "usage_poll snapshot mapping failed",
                            );
                        }
                    }
                }
                Err(err) => {
                    let status = classify_oauth_usage_error(&err);
                    // Pull the server-provided Retry-After out of the
                    // 429 variant so the next probe schedules against
                    // Anthropic's actual reset time rather than our
                    // local guess.
                    let retry_after = match &err {
                        forge_primitives::usage::oauth::OauthUsageError::RateLimited {
                            retry_after,
                        } => *retry_after,
                        _ => None,
                    };
                    self.accounts.lock().set_last_error(&key, status, retry_after);
                    // Branch the log message by error class so anyone
                    // reading triage logs gets the right framing.
                    // The previous shape said "persistent failures
                    // usually mean stale OAuth credentials" for every
                    // error variant - which misclassified 429
                    // rate-limits (a transient Anthropic throttle) as
                    // an auth/credentials issue and sent users hunting
                    // for /login problems that didn't exist.
                    let message: &str = match status {
                        account::UsageFetchStatus::RateLimited => {
                            "usage_poll fetch rate-limited by Anthropic; sub-second Retry-After is treated as 'no hint' and we back off exponentially"
                        }
                        account::UsageFetchStatus::Expired
                        | account::UsageFetchStatus::Unauthorized => {
                            "usage_poll fetch failed with auth error; OAuth credentials likely need refresh via /login"
                        }
                        account::UsageFetchStatus::NetworkFailed => {
                            "usage_poll fetch failed with network error; will retry on next tick"
                        }
                        account::UsageFetchStatus::Other => {
                            "usage_poll fetch failed with unhandled error class; see error field for details"
                        }
                    };
                    tracing::warn!(
                        target: "forge_workspace::account",
                        account = %key.0,
                        config_dir = %dir.display(),
                        error = %err,
                        retry_after_secs = ?retry_after.map(|d| d.as_secs()),
                        status = ?status,
                        "{message}",
                    );
                }
            }
        }

        // Persist the snapshot to disk so the next forge launch's
        // launchpad picker has seed data even if Anthropic 429s the
        // first probe. Skipped when no probe succeeded this round -
        // no point rewriting the file with the same contents.
        if any_success {
            let snapshots = self.accounts.lock().snapshots_for_cache();
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

    /// Read the cached usage snapshot for an account by display
    /// name. `None` when the poller hasn't yet succeeded (cold
    /// cache, no credentials, network blip). The TUI bottom panel
    /// renders the 5h / 7d bars from this snapshot.
    pub fn usage_for(&self, display_name: &str) -> Option<forge_primitives::usage::UsageSnapshot> {
        self.accounts.lock().usage(&AccountKey(display_name.to_owned())).cloned()
    }

    /// Read the last poll-attempt failure for an account, if any.
    /// `None` when the most recent poll succeeded (or no attempt
    /// has been made yet). The TUI bottom panel renders a DIM hint
    /// next to the `5h` / `7d` label when this is `Some` so the
    /// user can tell an empty bar from an upstream failure (the
    /// HTTP 429 case is especially common when multiple forge
    /// instances poll the same Anthropic account).
    pub fn usage_error_for(&self, display_name: &str) -> Option<crate::account::UsageFetchStatus> {
        self.accounts.lock().usage_error(&AccountKey(display_name.to_owned()))
    }

    /// Resolve an account `display_name` to its `(AccountKey,
    /// config_dir)` for the `/account` switch re-spawn. `None` when the
    /// name isn't a configured account (defensive - the picker only
    /// offers known accounts).
    pub(crate) fn resolve_account_for_switch(
        &self,
        display_name: &str,
    ) -> Option<(AccountKey, PathBuf)> {
        let accounts = self.accounts.lock();
        let key = AccountKey(display_name.to_owned());
        let config_dir = accounts.config_dir(&key)?.clone();
        Some((key, config_dir))
    }

    /// Snapshot the accounts a project may switch to, in allow-list
    /// order, each carrying its live rate-limit state for the
    /// `/account` picker. `allowed_accounts` is the project's
    /// forge.toml pin; empty falls back to every configured account
    /// (matching `pick_for_project`'s resolution). `current_account`
    /// is the session's active account display name, used to mark the
    /// current row. Returns owned [`crate::AccountRow`]s so the TUI
    /// holds a snapshot rather than the `AccountStateMap` lock.
    pub fn project_accounts_snapshot(
        &self,
        allowed_accounts: &[String],
        current_account: Option<&str>,
    ) -> Vec<crate::AccountRow> {
        let accounts = self.accounts.lock();
        // Resolve the allow-list to concrete account names, falling
        // back to every configured account when the project pins none.
        // Experimental accounts are then unioned in regardless of the
        // org pin (deduped) - they are excluded from auto-assignment but
        // globally selectable in the picker.
        let mut names: Vec<String> = if allowed_accounts.is_empty() {
            accounts.ordered_keys.iter().map(|k| k.0.clone()).collect()
        } else {
            allowed_accounts.to_vec()
        };
        for key in &accounts.ordered_keys {
            if accounts.is_experimental(key) && !names.contains(&key.0) {
                names.push(key.0.clone());
            }
        }
        let mut rows: Vec<crate::AccountRow> = names
            .into_iter()
            .filter_map(|name| {
                let key = AccountKey(name.clone());
                let config_dir = accounts.config_dir(&key)?.clone();
                let usable = accounts.is_account_usable(&key);
                let is_current = current_account == Some(name.as_str());
                let experimental = accounts.is_experimental(&key);
                let (five_hour_util, seven_day_util, resets_at) =
                    accounts.usage(&key).map_or((0.0, 0.0, None), |snapshot| {
                        (
                            account::five_hour_util(snapshot),
                            account::seven_day_util(snapshot),
                            account::binding_reset_at(snapshot),
                        )
                    });
                Some(crate::AccountRow {
                    display_name: name,
                    config_dir,
                    is_current,
                    usable,
                    five_hour_util,
                    seven_day_util,
                    resets_at,
                    experimental,
                })
            })
            .collect();
        // Stable-sort so regular rows lead and experimental rows trail,
        // matching the picker's EXPERIMENTAL group. `false` sorts before
        // `true`, and the sort preserves within-group order.
        rows.sort_by_key(|row| row.experimental);
        rows
    }

    /// Resolves a `SessionTarget` to the `SessionKey` used to look up
    /// the pool. For project-rooted targets (`Default` / `Named`) with
    /// no on-disk session for the project, returns a project-keyed
    /// placeholder (`__fresh__:<project_key>`) so re-entry stays
    /// idempotent against the same fresh-session intent. For `Named`
    /// with no matching project, returns
    /// [`WorkspaceError::ProjectNotFound`].
    fn resolve_target(&self, target: &SessionTarget) -> Result<SessionKey, WorkspaceError> {
        match target {
            SessionTarget::Default => Ok(self.lead_session_key_for(self.config.default_project())),
            SessionTarget::Named(name) => {
                let project = self.find_project_by_name(name)?;
                Ok(self.lead_session_key_for(project))
            }
            SessionTarget::Session(key) => Ok(key.clone()),
            SessionTarget::FreshInProject { synth_key, .. } => Ok(synth_key.clone()),
        }
    }

    /// Resolve a target's project-level account pin (the
    /// `[[orgs]].accounts = [...]` list inherited from the project's
    /// org in `forge.toml`). Project-rooted targets read directly off
    /// the matching `LoadedProject`; session-id targets walk the
    /// catalog for the session's original cwd and resolve it to the
    /// owning project by longest-ancestor-prefix so a resumed session
    /// (including one in a worktree subdir) inherits the originating
    /// project's pin.
    ///
    /// Config-load guarantees every `LoadedProject.accounts` is
    /// non-empty. The session-id branch can still miss (catalog has
    /// no record, or cwd doesn't match any project) - those fall
    /// back to the default project's pin so the picker always has
    /// a non-empty list. This mirrors the "use what we know" intent
    /// rather than a global account fallback.
    fn project_accounts_for(&self, target: &SessionTarget) -> Vec<String> {
        match target {
            SessionTarget::Default => self.config.default_project().accounts.clone(),
            SessionTarget::Named(name) => self.find_project_by_name(name).map_or_else(
                |_| self.config.default_project().accounts.clone(),
                |p| p.accounts.clone(),
            ),
            SessionTarget::Session(key) => {
                let matched = self.session_cwd_for(key).and_then(|cwd| {
                    // A worktree cwd is a subdir of its project root, so
                    // resolve by ancestor-prefix (project_name_for_path)
                    // instead of exact equality before reading the pin.
                    self.project_name_for_path(&cwd)
                        .and_then(|name| self.find_project_by_name(&name).ok())
                        .map(|p| p.accounts.clone())
                });
                matched.unwrap_or_else(|| self.config.default_project().accounts.clone())
            }
            // A fresh worker spawn's project_key is always the parent
            // project's (the worktree is created post-spawn via
            // --worktree; resume takes the Session branch), so this exact
            // key match resolves correctly.
            SessionTarget::FreshInProject { project_key, .. } => self
                .config
                .projects
                .iter()
                .find(|p| {
                    forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                        &p.path.to_string_lossy(),
                    )) == project_key.as_str()
                })
                .map_or_else(
                    || self.config.default_project().accounts.clone(),
                    |p| p.accounts.clone(),
                ),
        }
    }

    /// The project a spawn under `target` belongs to, for resolving
    /// that project's `[projects.<name>.env]`.
    ///
    /// Not [`Self::project_accounts_for`]'s resolution: that falls back
    /// to the default project on a miss, which would hand one project's
    /// keys to another's session, and it resolves through
    /// `session_cwd_for`, which misses every worker (the catalog holds
    /// no worker rows).
    fn project_for_target(&self, target: &SessionTarget) -> Option<LoadedProject> {
        match target {
            SessionTarget::Default => Some(self.config.default_project().clone()),
            SessionTarget::Named(name) => self.find_project_view_by_name(name),
            SessionTarget::Session(key) => self
                .cwd_for_session(key)
                .and_then(|cwd| self.project_name_for_path(&cwd))
                .and_then(|name| self.find_project_view_by_name(&name)),
            SessionTarget::FreshInProject { project_key, .. } => self
                .config
                .projects
                .iter()
                .find(|p| {
                    forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                        &p.path.to_string_lossy(),
                    )) == project_key.as_str()
                })
                .cloned(),
        }
    }

    /// Env for a spawn under `target` on the picked account: the
    /// account's merged `[env]` + `[accounts.env]` with the target
    /// project's `[projects.<name>.env]` layered on top.
    ///
    /// An unresolved target keeps the account env rather than
    /// borrowing the default project's. That case warns when any
    /// project declares env at all, because the symptom otherwise is
    /// a session silently missing keys its project declared.
    fn session_env_for(
        &self,
        target: &SessionTarget,
        account_env: &std::collections::HashMap<String, String>,
    ) -> std::collections::HashMap<String, String> {
        let Some(project) = self.project_for_target(target) else {
            if self.config.projects.iter().any(|p| !p.env.is_empty()) {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    event_name = "session_env_project_unresolved",
                    spawn_target = ?target,
                    "spawn target resolves to no project, so no [projects.<name>.env] is \
                     applied; the session gets only the account env",
                );
            }
            return account_env.clone();
        };
        // Logged even when the project declares nothing, so "resolved to
        // a project I did not mean" is visible.
        tracing::info!(
            target: "forge_workspace::workspace",
            event_name = "session_env_project_applied",
            project = %project.name,
            keys = %crate::config::LoadedConfig::applied_env_keys(Some(&project)),
            "resolved the spawn target to a project and applied its [projects.<name>.env]",
        );
        crate::config::session_env(Some(&project), account_env)
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

    /// Map a project to the `SessionKey` of its lead (most-recent)
    /// session, or to a `__fresh__:<project_key>` placeholder when the
    /// project has nothing on disk yet.
    fn lead_session_key_for(&self, project: &LoadedProject) -> SessionKey {
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                &project.path.to_string_lossy(),
            )));
        self.try_lead_session_id_for(project).unwrap_or_else(|| {
            SessionKey::from_session_id(format!("__fresh__:{}", project_key.as_str()))
        })
    }

    /// Return the project's lead (most-recent) session id when the
    /// on-disk catalog has one, else `None`. Drives the resume-first
    /// behaviour in [`Self::get_agent_handle`]: project-rooted targets
    /// (`Default` / `Named`) resume the lead when it exists and fall
    /// back to a fresh session otherwise. Picks via [`resolve_lead_session`]:
    /// latest `forge:lead`-tagged session beats untagged; `forge:worker:*`
    /// sessions are skipped entirely.
    fn try_lead_session_id_for(&self, project: &LoadedProject) -> Option<SessionKey> {
        let key = ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
            Some(&project.path.to_string_lossy()),
        ));
        let catalog = self.catalog.lock();
        let entries = catalog.get(&key)?;
        let lead = resolve_lead_session(entries)?;
        Some(SessionKey::from_session_id(lead.session_id.clone()))
    }

    /// Resume target for a project-rooted spawn: the project's catalog
    /// `lead`, unless `--new` (`force_new`) forces a fresh session.
    /// `Some(lead)` => resume that session; `None` => start fresh
    /// (`new_session`). `force_new` overrides a present lead - that is
    /// what makes the boot wave's leads come up fresh under `--new`,
    /// while every non-boot spawn leaves `force_new` false and resumes.
    fn apply_force_new_gate(lead: Option<SessionKey>, force_new: bool) -> Option<SessionKey> {
        if force_new { None } else { lead }
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

    /// Locate the parent project of a given `session_id` by walking
    /// the catalog. Used by `Command::SpawnSession` to seed the
    /// spawning bucket's cwd from the session's owning project before
    /// the agent boots.
    pub(crate) fn find_project_for_session(
        &self,
        session_key: &SessionKey,
    ) -> Option<LoadedProject> {
        let catalog = self.catalog.lock();
        let owning_project_key = catalog.iter().find_map(|(project_key, entries)| {
            if entries.iter().any(|e| e.session_id == session_key.as_str()) {
                Some(project_key.clone())
            } else {
                None
            }
        })?;
        drop(catalog);
        for project in &self.config.projects {
            let key =
                ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
                    Some(&project.path.to_string_lossy()),
                ));
            if key == owning_project_key {
                return Some(project.clone());
            }
        }
        None
    }

    /// Internal accessor for the SessionUpdate fan-in sender. Used
    /// by `spawn.rs` to emit `Spawning` / `ConnectionFailed` /
    /// `FatalError` from the App-level handlers.
    pub(crate) fn update_tx(&self) -> &mpsc::UnboundedSender<SessionUpdate> {
        &self.update_tx
    }

    /// Look up a session's recorded cwd by id. `claude --resume`
    /// indexes by the project key derived from the subprocess's
    /// working directory, so every explicit-resume code path must
    /// spawn the subprocess in the session's original cwd or the
    /// resume hits "No conversation found with session ID ..." even
    /// when the `.jsonl` exists. Returns `None` when the catalog has
    /// no entry for `session_id` or the entry lacks a cwd.
    pub fn session_cwd_for(&self, session_id: &SessionKey) -> Option<String> {
        self.catalog
            .lock()
            .values()
            .flatten()
            .find(|info| info.session_id == session_id.as_str())
            .and_then(|info| info.cwd.clone())
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
    pub fn domain_session_for(&self, key: &SessionKey) -> Option<Arc<Mutex<DomainSession>>> {
        self.domain_handles.lock().get(key).cloned()
    }

    /// Whether the session at `key` currently has a live agent
    /// handle stamped onto its [`DomainSession`]. Encapsulates the
    /// presence check so callers don't need to peek at
    /// `DomainSession.conn` directly - the field layout is a
    /// workspace internal.
    pub fn has_agent_for(&self, key: &SessionKey) -> bool {
        self.domain_session_for(key).is_some_and(|d| d.lock().conn.is_some())
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
            let key = key.clone();
            let senders = self.command_senders.lock();
            if let Some(sender) = senders.get(&key) {
                // Stamp turn_pending only on the routed path (set + route
                // together) so the /account backstop can't race a Prompt
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
                crate::session_task::execute_command_via_handle(&handle, &key, sid.as_deref(), cmd)
                    .map_err(|_| DispatchError::SessionClosed(key))
            }
            #[cfg(not(any(test, feature = "testing")))]
            {
                let _ = cmd;
                Err(DispatchError::UnknownSession(key))
            }
        } else {
            // App-level commands. The `spawn::*` handlers are sync -
            // they emit one event, kick off `get_agent_handle_with_spawn_key`
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
                Command::SpawnSession { session_id, launch_settings } => {
                    let span = tracing::info_span!(
                        "spawn_session",
                        session_id = %session_id,
                    );
                    let _enter = span.enter();
                    spawn::handle_spawn_session(self, &session_id, launch_settings);
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
                    spawned_by_session_id,
                    resume_existing,
                    kick,
                    return_to,
                } => {
                    let span = tracing::info_span!(
                        "spawn_worker",
                        project = %project_key.as_str(),
                        label = %label,
                        resume = resume_existing.is_some(),
                    );
                    let _enter = span.enter();
                    spawn::handle_spawn_worker(
                        self,
                        project_key,
                        &label,
                        charter,
                        spawned_by_session_id,
                        resume_existing,
                        kick,
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
                        target = %target_lead_key.as_str(),
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
                Command::SwitchAccount { key, account_display_name, launch_settings } => {
                    let span = tracing::info_span!(
                        "switch_account",
                        key = %key.as_str(),
                        account = %account_display_name,
                    );
                    let _enter = span.enter();
                    spawn::handle_switch_account(self, key, &account_display_name, launch_settings);
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

    /// Dispatch one `Command::SpawnWorker` per configured role for a
    /// project whose lead session just emitted `Connected`. Called
    /// from `SessionTask`'s Connected arm (lead path only, gated on
    /// `live_workers.is_empty()` so a reconnect / second-Connected
    /// doesn't double-spawn).
    ///
    /// `resume_map` carries `role.label -> session_id` entries for
    /// roles whose previous-process session JSONL was discovered by
    /// the team Connected hook's catalog scan. A role present in the
    /// map resumes that session; a role absent from the map spawns
    /// fresh. Empty map = all-fresh (the v1 behaviour preserved by
    /// `spawn_team_for_lead` for callers that haven't migrated yet).
    ///
    /// The dispatcher reuses the existing `Command::SpawnWorker`
    /// handler (`handle_spawn_worker`); the only difference from
    /// MCP-tool-driven spawn is the absence of an MCP caller -
    /// `spawned_by_session_id` is the lead's UUID directly, and
    /// `return_to` is a dropped oneshot (we don't await the reply
    /// here; each worker's own Connected event surfaces them via
    /// the normal flow).
    ///
    /// No-op when `team` is empty.
    pub(crate) fn spawn_team_for_lead_with_resume(
        self: &Arc<Self>,
        lead_session_id: &str,
        project_key: &crate::target::ProjectKey,
        team: &[crate::team::Role],
        resume_map: &std::collections::HashMap<String, String>,
    ) {
        if team.is_empty() {
            return;
        }
        for role in team {
            let resume_existing = resume_map.get(&role.label).cloned();
            let (tx, _rx) = tokio::sync::oneshot::channel();
            let cmd = crate::protocol::Command::SpawnWorker {
                project_key: project_key.clone(),
                label: role.label.clone(),
                charter: role.charter.clone(),
                spawned_by_session_id: lead_session_id.to_owned(),
                resume_existing,
                kick: None,
                return_to: tx,
            };
            if let Err(err) = self.dispatch(cmd) {
                tracing::error!(
                    target: "forge_workspace::team",
                    project = %project_key.as_str(),
                    label = %role.label,
                    error = ?err,
                    "spawn_team_for_lead: dispatch failed for label"
                );
            }
        }
    }

    /// Re-spawn this project's persisted dynamic (LLM-spawned) workers on
    /// lead reconnect. Folded into the same catalog-scan trigger as the
    /// static roster and sharing its `resume_map`, so every downstream
    /// mechanic (resume-by-label, `resume_existing`, the past-progress
    /// guard) is identical. Charter + kick come from the DB row rather
    /// than files.
    ///
    /// Since this holds the dynamic set, it decides the kick directly:
    /// a resume gets the forge restart note (dynamic workers have no
    /// `resume-kick.md`), telling it to continue rather than restart; a
    /// fresh re-spawn (never prompted, so no catalog tag) re-delivers the
    /// stored kick. Both flow through `maybe_kick_worker_on_connected`'s
    /// inline-kick path, so that hook needs no per-worker store lookup.
    pub(crate) fn spawn_dynamic_workers_for_lead(
        self: &Arc<Self>,
        lead_session_id: &str,
        project_key: &crate::target::ProjectKey,
        dynamic: &[crate::store::dynamic_workers::DynamicWorker],
        resume_map: &std::collections::HashMap<String, String>,
    ) {
        for worker in dynamic {
            let resume_existing = resume_map.get(&worker.label).cloned();
            let kick = if resume_existing.is_some() {
                Some(DYNAMIC_WORKER_RESTART_NOTE.to_owned())
            } else {
                worker.kick.clone()
            };
            let (tx, _rx) = tokio::sync::oneshot::channel();
            let cmd = crate::protocol::Command::SpawnWorker {
                project_key: project_key.clone(),
                label: worker.label.clone(),
                charter: worker.charter.clone(),
                spawned_by_session_id: lead_session_id.to_owned(),
                resume_existing,
                kick,
                return_to: tx,
            };
            if let Err(err) = self.dispatch(cmd) {
                tracing::error!(
                    target: "forge_workspace::team",
                    project = %project_key.as_str(),
                    label = %worker.label,
                    error = ?err,
                    "spawn_dynamic_workers_for_lead: dispatch failed for label"
                );
            }
        }
    }

    /// Engineering-team Connected-hook entry point. Synchronously
    /// claims a per-project in-flight guard, then spawns an async
    /// task that scans the catalog for `forge:worker:<label>` tagged
    /// sessions and dispatches one `Command::SpawnWorker` per role
    /// (with `resume_existing` populated for roles that have a
    /// matching catalog entry, `None` otherwise). The guard is
    /// released after the dispatches go out so a fast double-
    /// Connected can't slip a second scan through.
    ///
    /// No-op when the per-project guard is already claimed (another
    /// scan is in flight). The first-pass `live_workers.is_empty()`
    /// gate in `session_task::maybe_spawn_team_on_connected` catches
    /// the post-scan case; this guard covers the during-scan window.
    /// Load every label's charter + initial kick (file-driven loader)
    /// before spawning. Returns the loaded set, skipping (with a warn
    /// log) any label whose files are missing - so a single bad label
    /// in `static_workers = [...]` doesn't block the rest of the roster.
    fn load_team_roles(
        team: &[String],
        namespace: &str,
        project_key: &crate::target::ProjectKey,
    ) -> Vec<crate::team::Role> {
        let mut loaded: Vec<crate::team::Role> = Vec::with_capacity(team.len());
        for label in team {
            match crate::team::Role::load_for(label, namespace) {
                Ok(role) => loaded.push(role),
                Err(err) => {
                    tracing::warn!(
                        target: "forge_workspace::team",
                        project = %project_key.as_str(),
                        label = %label,
                        error = %err,
                        "no charter/kick for worker label (project-first then global); spawn skipped. Populate ~/.claude/forge-team/<project>/<label>/ or ~/.claude/forge-team/<label>/ (copy from docs/forge-team-defaults/) or use workers__create_role."
                    );
                }
            }
        }
        loaded
    }

    pub(crate) fn spawn_team_for_lead_with_catalog_scan(
        self: &Arc<Self>,
        lead_session_id: String,
        project_key: crate::target::ProjectKey,
        project_dir: PathBuf,
        namespace: String,
        static_workers: Vec<String>,
        force_new: bool,
    ) {
        // Persisted dynamic workers re-spawn alongside the config roster
        // through the same resume-map trigger; charter/kick come from
        // their DB row rather than files.
        let dynamic = self.dynamic_workers_for_project(&project_key);
        if static_workers.is_empty() && dynamic.is_empty() {
            return;
        }
        if !self.try_claim_team_spawn(&project_key) {
            tracing::debug!(
                target: "forge_workspace::team",
                project = %project_key.as_str(),
                "worker-spawn already in flight; skipping duplicate Connected fire",
            );
            return;
        }
        // `--new`: the lead came up fresh, so its workers do too. Skip
        // the catalog resume scan entirely and spawn every worker fresh
        // (an empty resume map => `resume_existing = None` for all).
        if force_new {
            let loaded = Self::load_team_roles(&static_workers, &namespace, &project_key);
            let empty = std::collections::HashMap::new();
            self.spawn_team_for_lead_with_resume(&lead_session_id, &project_key, &loaded, &empty);
            self.spawn_dynamic_workers_for_lead(&lead_session_id, &project_key, &dynamic, &empty);
            self.release_team_spawn(&project_key);
            return;
        }
        // When invoked inside a tokio runtime (production + any
        // `#[tokio::test]`), spawn the catalog scan + dispatch
        // asynchronously so translate_event isn't blocked on file
        // I/O. When invoked outside a runtime (the sync `#[test]`
        // fixtures in `team_hook_tests`), fall back to a synchronous
        // dispatch with an empty resume map: those tests exercise
        // the worker-fanout shape, not the resume mechanic. Tests that
        // need the resume path opt into `#[tokio::test]` + fixture
        // JSONLs explicitly.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                target: "forge_workspace::team",
                project = %project_key.as_str(),
                "no tokio runtime in scope; falling back to sync worker-spawn (test path)",
            );
            let loaded = Self::load_team_roles(&static_workers, &namespace, &project_key);
            let empty = std::collections::HashMap::new();
            self.spawn_team_for_lead_with_resume(&lead_session_id, &project_key, &loaded, &empty);
            self.spawn_dynamic_workers_for_lead(&lead_session_id, &project_key, &dynamic, &empty);
            self.release_team_spawn(&project_key);
            return;
        };
        let workspace = Arc::clone(self);
        let config_dirs = {
            let mut dirs = self.accounts.lock().config_dirs();
            if !dirs.contains(&self.config_dir) {
                dirs.push(self.config_dir.clone());
            }
            dirs
        };
        handle.spawn(async move {
            let resume_map = scan_worker_resume_map(&config_dirs, &project_dir).await;
            let loaded = Self::load_team_roles(&static_workers, &namespace, &project_key);
            tracing::info!(
                target: "forge_workspace::team",
                project = %project_key.as_str(),
                lead_session_id = %lead_session_id,
                resume_count = resume_map.len(),
                fresh_count = loaded.len().saturating_sub(resume_map.len()),
                missing_count = static_workers.len().saturating_sub(loaded.len()),
                dynamic_count = dynamic.len(),
                "worker-spawn catalog scan complete; dispatching SpawnWorker per label",
            );
            workspace.spawn_team_for_lead_with_resume(
                &lead_session_id,
                &project_key,
                &loaded,
                &resume_map,
            );
            workspace.spawn_dynamic_workers_for_lead(
                &lead_session_id,
                &project_key,
                &dynamic,
                &resume_map,
            );
            workspace.release_team_spawn(&project_key);
        });
    }

    /// Claim the per-project team-spawn in-flight guard. Returns true
    /// if the guard was acquired (entry was absent), false if another
    /// scan was already in flight.
    fn try_claim_team_spawn(&self, project_key: &crate::target::ProjectKey) -> bool {
        self.team_spawn_in_flight.lock().insert(project_key.clone())
    }

    /// Release the per-project team-spawn in-flight guard. Paired with
    /// `try_claim_team_spawn`; called from the async task's tail
    /// after `spawn_team_for_lead_with_resume` returns.
    fn release_team_spawn(&self, project_key: &crate::target::ProjectKey) {
        self.team_spawn_in_flight.lock().remove(project_key);
    }

    /// Inspect a worker session's JSONL to decide whether it has
    /// progressed beyond the initial team-kick. Counts user-role
    /// turns; threshold is 2.
    ///
    /// - 0 user turns: fresh session, no kick fired yet (or JSONL not
    ///   yet written). Re-fire the kick.
    /// - 1 user turn: kick landed but worker didn't progress past it
    ///   (forge restarted before the worker did any real work, or
    ///   crashed mid-response). Re-fire the kick so the work
    ///   actually starts.
    /// - 2+ user turns: worker received the kick AND has been
    ///   prompted again since (peer/worker message, lead follow-up).
    ///   Leave it alone; re-firing would override its in-flight
    ///   state.
    ///
    /// Returns true iff the worker has 2+ user turns. The kick path
    /// gates on this only for resume sessions; fresh sessions skip
    /// the check (their JSONL doesn't exist yet so the answer is
    /// always false anyway).
    pub(crate) fn worker_has_progress_past_kick(&self, session_id: &str) -> bool {
        let session_key = SessionKey::from_session_id(session_id.to_owned());
        let config_dir =
            self.config_dir_for(&session_key).unwrap_or_else(|| self.config_dir.clone());
        let messages = forge_agent::userdata::catalog::scan::get_session_messages(
            &config_dir,
            session_id,
            None,
        );
        let user_turn_count = messages
            .iter()
            .filter(|m| matches!(m.kind, forge_primitives::SessionMessageKind::User))
            .count();
        user_turn_count >= 2
    }

    /// Set the `session_id` field on the workspace's `DomainSession`
    /// for `key`. No-op when no domain handle is registered for
    /// `key`. Used by `App::set_session_id` to stamp the
    /// claude-issued UUID once the first `Connected` event fires -
    /// the workspace consults this when routing `AgentHandle` calls
    /// that take a session_id.
    pub fn set_session_id_in_domain(
        &self,
        key: &SessionKey,
        value: Option<forge_primitives::SessionId>,
    ) {
        let Some(domain) = self.domain_handles.lock().get(key).cloned() else {
            return;
        };
        domain.lock().session_id = value;
    }

    /// Graceful shutdown of every pooled Agent. Drains the pool, then
    /// drops each `Arc<AgentHandle>` so the underlying
    /// `forge_sdk::Client` kills its `claude` subprocess via its
    /// existing `Drop` impl when the last reference goes away.
    ///
    /// forge-tui releases its handle reference before calling shutdown,
    /// so Workspace is the sole owner of every pool entry and dropping
    /// it triggers the subprocess shutdown chain (sender drop ->
    /// dispatcher exit -> Client drop -> subprocess kill_on_drop).
    /// Callers that hold cloned handles across shutdown will need to
    /// release them for the kill-chain to fire promptly.
    pub fn shutdown(&self) {
        // Drop command senders first so every SessionTask sees its
        // command channel close and exits cleanly.
        let _ = self.command_senders.lock().drain().collect::<Vec<_>>();
        let _ = self.domain_handles.lock().drain().collect::<Vec<_>>();
        let entries: Vec<_> = self.pool.lock().drain().collect();
        // Each (SessionKey, PooledAgent) drops here; the subprocess
        // teardown chain (sender drop -> dispatcher exit -> Client
        // drop -> subprocess kill_on_drop) is synchronous and fast.
        drop(entries);
    }

    /// Release a single session's pool entry. Drops the workspace's
    /// `Arc<AgentHandle>` for that key so the underlying `claude`
    /// subprocess exits once the consumer (forge-tui's bucket) also
    /// **Cascade-aware** lead release. Use this when closing a project's
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
    pub fn release_session_with_cascade(&self, session_key: &SessionKey) {
        let cascade_project = self.list_projects().into_iter().find(|view| {
            let in_catalog = view.sessions.iter().any(|s| s.session == *session_key);
            let is_worker =
                self.list_live_workers(&view.key).iter().any(|w| w.session_key == *session_key);
            in_catalog && !is_worker
        });
        let cascade_project = cascade_project.map(|view| view.key);
        if let Some(project_key) = cascade_project {
            for entry in self.drain_live_workers(&project_key) {
                let status = entry.to_status();
                let is_git_repo_at_spawn = entry.is_git_repo_at_spawn;
                let _ = self.update_tx.send(SessionUpdate::WorkerStatusChanged {
                    project_key: project_key.clone(),
                    action: crate::protocol::WorkerStatusAction::Removed,
                    status,
                    is_git_repo_at_spawn,
                });
                self.release_session(&entry.session_key);
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
    /// - `handle_close_worker` (per-row worker X click) - cascade
    ///   must NOT fire because the worker was already removed from
    ///   `live_workers` by `remove_latest_worker`; the cascade-check
    ///   in `release_session_with_cascade` would misidentify the
    ///   orphaned session_key as a lead and drain every OTHER worker.
    /// - The lead-cascade loop inside `release_session_with_cascade`
    ///   itself, when releasing each worker.
    /// - Synthetic spawn-key cleanup (launchpad retry path).
    ///
    /// Use `release_session_with_cascade` instead when the caller is
    /// the lead-row close gesture.
    pub(crate) fn release_session(&self, session_key: &SessionKey) {
        let removed = self.pool.lock().remove(session_key);
        drop(removed);
        let _ = self.command_senders.lock().remove(session_key);
        let _ = self.domain_handles.lock().remove(session_key);
    }

    /// Supersession-safe release: drop `session_key`'s pool entry,
    /// command sender, and domain handle ONLY when the pooled agent is
    /// still `handle` (by `Arc` identity). A SessionTask whose session
    /// was re-spawned under the same key (an `/account` switch) is a
    /// stale predecessor - when it exits and runs this cleanup, the
    /// pool already holds the successor's handle, so the guard no-ops
    /// and the live re-spawned session is left intact. Gating all three
    /// maps on the one identity check keeps a superseded task from
    /// half-cleaning its successor.
    pub(crate) fn release_session_if_current(
        &self,
        session_key: &SessionKey,
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

    /// Lock the durable cron list, apply `f`, and persist to the
    /// machine-local store. Every cron-list mutation routes through here -
    /// `cron__create` / `cron__delete`, the scheduler's fire-advance, and
    /// boot catch-up - so the in-memory set and the store never diverge.
    pub(crate) fn with_crons_mut<R>(
        &self,
        f: impl FnOnce(&mut Vec<forge_primitives::CronEntry>) -> R,
    ) -> R {
        let mut crons = self.crons.lock();
        let result = f(&mut crons);
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::cron::replace_all(db, &crons)
        {
            tracing::error!(
                target: "forge_workspace::workspace",
                %error,
                "persisting crons to the store failed; a scheduled cron may be lost on restart",
            );
        }
        result
    }

    /// Append a cron and persist. Backs `cron__create`.
    pub(crate) fn push_cron(&self, entry: forge_primitives::CronEntry) {
        self.with_crons_mut(|crons| crons.push(entry));
    }

    /// Remove the cron `id` in `project_name` regardless of owner, persist,
    /// and report whether an entry was removed. Backs the fire-router's
    /// owner-gone removal; the owner-scoped `cron__delete` uses
    /// [`Self::remove_cron_owned_by`].
    pub(crate) fn remove_cron(&self, project_name: &str, id: &forge_primitives::CronId) -> bool {
        self.with_crons_mut(|crons| {
            let before = crons.len();
            crons.retain(|c| !(c.id == *id && c.project_name == project_name));
            crons.len() != before
        })
    }

    /// Remove the cron `id` in `project_name` only when its owner matches
    /// `owner` (`None` = a lead cron, `Some(label)` = that worker's),
    /// persist, and report whether an entry was removed. Backs the
    /// owner-scoped `cron__delete` so a caller deletes only its own crons.
    pub(crate) fn remove_cron_owned_by(
        &self,
        project_name: &str,
        id: &forge_primitives::CronId,
        owner: Option<&str>,
    ) -> bool {
        self.with_crons_mut(|crons| {
            let before = crons.len();
            crons.retain(|c| {
                !(c.id == *id && c.project_name == project_name && c.team_role.as_deref() == owner)
            });
            crons.len() != before
        })
    }

    /// Remove worker `label`'s crons in `project_key` from the in-memory
    /// set and the store, scoped to `(project name, team_role == Some(label))`.
    /// Backs `spawn::teardown_worker` so a despawned dynamic worker's crons
    /// go with its durable row. The key resolves to a name via the same view
    /// lookup `remove_gotify_subscriptions_for_worker` uses.
    pub(crate) fn delete_crons_for_worker(&self, project_key: &ProjectKey, label: &str) {
        let Some(project_name) =
            self.list_projects().into_iter().find(|v| v.key == *project_key).map(|v| v.name)
        else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                project = %project_key.as_str(),
                label = %label,
                "could not resolve a project name at worker teardown; its crons may be stranded",
            );
            return;
        };
        self.with_crons_mut(|crons| {
            crons.retain(|c| {
                !(c.project_name == project_name && c.team_role.as_deref() == Some(label))
            });
        });
    }

    /// The crons registered for `project_name`. Backs `cron__list` and
    /// the Inspector SCHEDULES snapshot, which scopes by the active tab's
    /// stamped project name.
    pub fn crons_for_project(&self, project_name: &str) -> Vec<forge_primitives::CronEntry> {
        self.crons.lock().iter().filter(|c| c.project_name == project_name).cloned().collect()
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

    /// A snapshot of every cron across all projects. Backs the
    /// scheduler's per-tick due-check.
    pub(crate) fn all_crons_snapshot(&self) -> Vec<forge_primitives::CronEntry> {
        self.crons.lock().clone()
    }

    /// Buffer a cron prompt for an asleep owner, keyed by
    /// `(project, team_role)`; drained on the owner's first `Connected`.
    pub(crate) fn buffer_cron_for_owner(
        &self,
        project: &str,
        team_role: Option<&str>,
        text: String,
        missed: bool,
    ) {
        self.pending_cron_by_owner
            .lock()
            .entry((project.to_owned(), team_role.map(str::to_owned)))
            .or_default()
            .push(PendingCron { text, missed });
    }

    /// Take (and clear) the cron prompts buffered for the connecting
    /// session's owner - `(project of cwd, the session's worker label, or
    /// None for a lead)`. Empty when nothing was buffered or the cwd is
    /// under no project.
    pub(crate) fn take_pending_crons_for_session(
        &self,
        session_key: &SessionKey,
        cwd: &str,
    ) -> Vec<PendingCron> {
        let Some(project) = self.project_name_for_path(cwd) else { return Vec::new() };
        let team_role = self.worker_label_for_session(session_key);
        self.pending_cron_by_owner.lock().remove(&(project, team_role)).unwrap_or_default()
    }

    /// The worker label owning `session_key` across all projects, or `None`
    /// when it is not a live worker (a lead or other session).
    pub(crate) fn worker_label_for_session(&self, session_key: &SessionKey) -> Option<String> {
        self.live_workers
            .lock()
            .values()
            .flatten()
            .find(|w| w.session_key == *session_key)
            .map(|w| w.label.clone())
    }

    /// Register a Gotify subscription in the active set. Durable ones
    /// (lead / team-worker) also persist to the redb store; ephemeral
    /// ad-hoc-worker ones stay in memory only and drop on restart.
    pub(crate) fn add_gotify_subscription(
        &self,
        sub: forge_primitives::GotifySubscription,
        durable: bool,
    ) {
        if durable
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::gotify::insert(db, &sub)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "persisting a Gotify subscription failed",
            );
        }
        self.gotify_subs.lock().push(sub);
    }

    /// Remove the subscription `id` in `project` only when its owner
    /// matches `owner` (`None` = a lead subscription, `Some(label)` =
    /// that worker's), from both the active set and (when present) the
    /// redb store. Returns whether an entry was removed. Backs the
    /// owner-scoped `gotify__unsubscribe` so a caller removes only what
    /// it subscribed, mirroring [`Self::remove_cron_owned_by`]. Worker
    /// teardown uses [`Self::remove_gotify_subscriptions_for_worker`]
    /// instead and is deliberately not owner-gated.
    pub(crate) fn remove_gotify_subscription_owned_by(
        &self,
        project: &str,
        id: uuid::Uuid,
        owner: Option<&str>,
    ) -> bool {
        let removed = {
            let mut subs = self.gotify_subs.lock();
            let before = subs.len();
            subs.retain(|s| {
                !(s.id == id && s.project == project && s.team_role.as_deref() == owner)
            });
            subs.len() != before
        };
        if removed
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::gotify::remove(db, id)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                "removing a persisted Gotify subscription failed",
            );
        }
        removed
    }

    /// Remove the worker `label`'s Gotify subscriptions in `project_key`
    /// from both the active set and the redb store, scoped to
    /// `(project name, team_role == Some(label))`. Backs
    /// `spawn::teardown_worker` so a despawned durable dynamic worker can't
    /// orphan a persisted sub. Subscriptions are project-scoped by NAME, so
    /// the key is resolved to a name via the same view lookup
    /// `resolve_identity` uses when subscribing.
    pub(crate) fn remove_gotify_subscriptions_for_worker(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) {
        let Some(project_name) =
            self.list_projects().into_iter().find(|v| v.key == *project_key).map(|v| v.name)
        else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                project = %project_key.as_str(),
                label = %label,
                "could not resolve a project name at worker teardown; its Gotify subs may be stranded",
            );
            return;
        };
        let removed_ids: Vec<uuid::Uuid> = {
            let mut subs = self.gotify_subs.lock();
            let mut removed = Vec::new();
            subs.retain(|s| {
                let owned = s.project == project_name && s.team_role.as_deref() == Some(label);
                if owned {
                    removed.push(s.id);
                }
                !owned
            });
            removed
        };
        if removed_ids.is_empty() {
            return;
        }
        if let Some(db) = self.db.lock().as_ref() {
            for id in removed_ids {
                if let Err(error) = crate::store::gotify::remove(db, id) {
                    // Same zombie-durable-state class as a stranded
                    // dynamic-worker row: match its error! severity.
                    tracing::error!(
                        target: "forge_workspace::workspace",
                        %error,
                        id = %id,
                        project = %project_name,
                        label = %label,
                        "deleting a persisted Gotify subscription failed; it reloads into the active set on restart and a future same-label worker inherits it",
                    );
                }
            }
        }
    }

    /// The active subscriptions for `project`. Backs `gotify__list` and
    /// the Inspector GOTIFY snapshot, which scopes by the active tab's
    /// stamped project name (mirroring [`Self::crons_for_project`]).
    pub fn gotify_subscriptions_for_project(
        &self,
        project: &str,
    ) -> Vec<forge_primitives::GotifySubscription> {
        self.gotify_subs.lock().iter().filter(|s| s.project == project).cloned().collect()
    }

    /// Whether the Gotify stream is currently connected. Backs the
    /// Inspector GOTIFY status line.
    pub fn gotify_connected(&self) -> bool {
        *self.gotify_connected.lock()
    }

    /// Persist a dynamic (LLM-spawned) worker's re-spawn args to the redb
    /// store so a forge restart can bring it back. Called only from the
    /// MCP `workers__spawn` path - boot/reconnect re-spawns must NOT
    /// persist. Returns `Err` when durability could not be achieved (the
    /// store isn't open, or the write failed) so the caller can warn the
    /// lead that this worker won't survive a restart.
    pub(crate) fn persist_dynamic_worker(
        &self,
        worker: &crate::store::dynamic_workers::DynamicWorker,
    ) -> anyhow::Result<()> {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            anyhow::bail!("the dynamic-worker store is unavailable this session");
        };
        crate::store::dynamic_workers::insert(db, worker)
    }

    /// Delete a dynamic worker's persisted row so it never re-spawns.
    /// Keyed by `(project_key, label)`; a no-op for static workers (they
    /// have no row) and when the DB isn't open. Called from
    /// `spawn::teardown_worker`, the shared close/despawn routine.
    pub(crate) fn delete_dynamic_worker(&self, project_key: &ProjectKey, label: &str) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) =
                crate::store::dynamic_workers::delete(db, project_key.as_str(), label)
        {
            // This is the last spot in the durability lifecycle that can
            // silently leave a zombie row (a despawned worker that
            // re-spawns on restart), so match persist's error! severity.
            tracing::error!(
                target: "forge_workspace::workspace",
                %error,
                project = %project_key.as_str(),
                label = %label,
                "deleting a persisted dynamic worker failed; it may re-spawn on restart",
            );
        }
    }

    /// Every persisted dynamic worker for `project_key`. Empty when the
    /// DB isn't open or the read fails. Backs the lead-reconnect re-spawn
    /// merge and the resume restart-note detection.
    pub(crate) fn dynamic_workers_for_project(
        &self,
        project_key: &ProjectKey,
    ) -> Vec<crate::store::dynamic_workers::DynamicWorker> {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            return Vec::new();
        };
        crate::store::dynamic_workers::list_for_project(db, project_key.as_str()).unwrap_or_else(
            |error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    project = %project_key.as_str(),
                    "listing persisted dynamic workers failed",
                );
                Vec::new()
            },
        )
    }

    /// Whether `label` has a persisted dynamic-worker row in `project_key`.
    /// Distinct from [`Self::dynamic_workers_for_project`], which swallows a
    /// read failure as empty: this surfaces the error (and treats a missing
    /// store as one) so a caller can tell "conclusively absent" from "could
    /// not read" - the cron fire router must not delete a cron on a hiccup.
    pub(crate) fn dynamic_worker_exists(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) -> anyhow::Result<bool> {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            anyhow::bail!("the dynamic-worker store is unavailable this session");
        };
        let rows = crate::store::dynamic_workers::list_for_project(db, project_key.as_str())?;
        Ok(rows.iter().any(|w| w.label == label))
    }

    /// Persisted review threads for `(project, branch)`, empty when the
    /// DB isn't open or the read fails. `project` is the forge.toml
    /// project NAME (worktree-agnostic). Query-style, so this is a direct
    /// method - review-thread persistence is local redb IO, not an
    /// agent-driving action that needs the `Command` bus.
    /// Load the persisted review threads for `(project, branch)`. `Ok`
    /// with an empty vec when the store isn't open or the branch has no
    /// row; `Err` with a display string when an existing row fails to
    /// decode / read, so the overlay can surface the failure instead of
    /// showing a silently-empty review pane.
    pub fn load_review_threads(
        &self,
        project: &str,
        branch: &str,
    ) -> Result<Vec<ReviewThread>, String> {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            return Ok(Vec::new());
        };
        crate::store::review::load(db, project, branch).map_err(|error| {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                project = %project,
                branch = %branch,
                "loading review threads failed",
            );
            format!("{error:#}")
        })
    }

    /// Overwrite the review-thread set for `(project, branch)`. Best-effort:
    /// a write failure is logged, not surfaced, since the diff overlay has
    /// no recovery path.
    pub fn save_review_threads(&self, project: &str, branch: &str, threads: &[ReviewThread]) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::review::save(db, project, branch, threads)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                project = %project,
                branch = %branch,
                "saving review threads failed",
            );
        }
    }

    /// Insert or replace one review thread by id in `(project, branch)`.
    /// Returns whether the write was confirmed - `false` when the store
    /// isn't open or the write failed, so the caller can leave the
    /// comment in the at-risk (not-yet-durable) bucket.
    pub fn upsert_review_thread(&self, project: &str, branch: &str, thread: ReviewThread) -> bool {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            return false;
        };
        match crate::store::review::upsert(db, project, branch, thread) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    project = %project,
                    branch = %branch,
                    "upserting a review thread failed",
                );
                false
            }
        }
    }

    /// Remove one review thread by id from `(project, branch)`, so a
    /// deleted comment does not resurrect on the next hydrate. Returns
    /// whether the removal was confirmed.
    pub fn remove_review_thread(&self, project: &str, branch: &str, id: &str) -> bool {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            return false;
        };
        match crate::store::review::remove_thread(db, project, branch, id) {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    project = %project,
                    branch = %branch,
                    id = %id,
                    "removing a review thread failed",
                );
                false
            }
        }
    }

    /// Set the status of one review thread by id, bumping its `updated_at`.
    pub fn set_review_thread_status(
        &self,
        project: &str,
        branch: &str,
        id: &str,
        status: ReviewStatus,
    ) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::review::set_status(db, project, branch, id, status)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                project = %project,
                branch = %branch,
                id = %id,
                "setting a review-thread status failed",
            );
        }
    }

    /// Delete the whole review-thread set for `(project, branch)`. Called
    /// on branch/worktree teardown so an abandoned or merged branch's
    /// threads don't linger.
    pub fn delete_review_threads(&self, project: &str, branch: &str) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::review::delete(db, project, branch)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                project = %project,
                branch = %branch,
                "deleting review threads failed",
            );
        }
    }

    /// Delete the whole review set for `(project, branch)`. Called beside
    /// [`Self::delete_review_threads`] on branch/worktree teardown so a
    /// reused branch doesn't inherit phantom reviews.
    pub fn delete_reviews(&self, project: &str, branch: &str) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::review::delete_reviews(db, project, branch)
        {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                project = %project,
                branch = %branch,
                "deleting reviews failed",
            );
        }
    }

    /// Load the submitted reviews for `(project, branch)`, oldest first.
    /// `Ok` with an empty vec when the store isn't open or the branch has
    /// no row; `Err` with a display string when an existing row fails to
    /// decode, so the overlay can surface the failure.
    pub fn load_reviews(
        &self,
        project: &str,
        branch: &str,
    ) -> Result<Vec<forge_primitives::ReviewSet>, String> {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            return Ok(Vec::new());
        };
        crate::store::review::load_reviews(db, project, branch).map_err(|error| {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                project = %project,
                branch = %branch,
                "loading reviews failed",
            );
            format!("{error:#}")
        })
    }

    /// The branches under `project` that hold submitted reviews. `Ok`
    /// with an empty vec when the store isn't open; `Err` on a read
    /// failure. `review__list` asks this only after its own branch came
    /// back empty, so a review filed against another branch can't read
    /// as "no reviews".
    pub fn review_branches(&self, project: &str) -> Result<Vec<String>, String> {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            return Ok(Vec::new());
        };
        crate::store::review::review_branches(db, project).map_err(|error| {
            tracing::warn!(
                target: "forge_workspace::workspace",
                %error,
                project = %project,
                "listing review branches failed",
            );
            format!("{error:#}")
        })
    }

    /// Seal a new review for `(project, branch)`, filing the listed
    /// still-unfiled threads into it and appending it with the optional
    /// summary. Records `origin` as the notice target for the branch (the
    /// reviewer's session, so a later worker review-turn pings it). Returns
    /// the minted [`forge_primitives::ReviewSet`] (`None` when the store
    /// isn't open or the write failed) so the caller can surface its number.
    pub fn submit_review(
        &self,
        project: &str,
        branch: &str,
        summary: Option<String>,
        thread_ids: &[String],
        origin: SessionKey,
    ) -> Option<forge_primitives::ReviewSet> {
        // Scope the `db` guard to the store write and drop it BEFORE taking
        // `review_origin` - `drain_review_activity` locks these in the
        // opposite order (`review_origin` then `db` via `review_list`), so
        // holding both here would risk an AB-BA deadlock on the concurrent
        // submit / worker-turn-end paths.
        let review = {
            let guard = self.db.lock();
            let db = guard.as_ref()?;
            match crate::store::review::submit_review(db, project, branch, summary, thread_ids) {
                Ok(review) => review,
                Err(error) => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        %error,
                        project = %project,
                        branch = %branch,
                        "submitting a review failed",
                    );
                    return None;
                }
            }
        };
        self.review_origin.lock().insert((project.to_owned(), branch.to_owned()), origin);
        Some(review)
    }

    /// The `review__list` view for `(project, branch)`: submitted reviews
    /// with per-review comment tallies, newest first. `Ok` (possibly empty)
    /// when the store is closed or the branch has no reviews; `Err` when a
    /// stored row fails to decode, so the MCP surfaces the failure instead
    /// of a silent empty list.
    pub fn review_list(
        &self,
        project: &str,
        branch: &str,
    ) -> Result<Vec<crate::mcp::review::ReviewSummary>, String> {
        let reviews = self.load_reviews(project, branch)?;
        let threads = self.load_review_threads(project, branch)?;
        Ok(crate::mcp::review::summarize(&reviews, &threads))
    }

    /// The `review__get` view for one review on `(project, branch)`: its
    /// overview plus the comments filed under it, each carrying the
    /// anchored code. `Ok(None)` when no such review; `Err` on a decode
    /// failure.
    pub fn review_get(
        &self,
        project: &str,
        branch: &str,
        review_id: &str,
    ) -> Result<Option<crate::mcp::review::ReviewDetail>, String> {
        let reviews = self.load_reviews(project, branch)?;
        let threads = self.load_review_threads(project, branch)?;
        Ok(crate::mcp::review::detail(&reviews, &threads, review_id))
    }

    /// Append a worker reply to `comment_id` on `(project, branch)` and
    /// return the thread's status after the append (Open -> Addressed;
    /// Resolved / Outdated unchanged). Records the reply in `caller`'s
    /// turn activity buffer so the reviewer is notified at turn end. `Err`
    /// when the store is closed, the write fails, or no comment with that
    /// id exists in this scope, so a stale or cross-branch id is rejected
    /// rather than silently ignored.
    pub fn review_reply(
        &self,
        caller: &SessionKey,
        project: &str,
        branch: &str,
        comment_id: &str,
        author_label: &str,
        text: &str,
        at: &str,
    ) -> Result<ReviewStatus, String> {
        let status = {
            let guard = self.db.lock();
            let db = guard.as_ref().ok_or_else(|| "review store is unavailable".to_owned())?;
            crate::store::review::append_reply(
                db,
                project,
                branch,
                comment_id,
                author_label,
                text,
                at,
            )
            .map_err(|error| {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    %error,
                    project = %project,
                    branch = %branch,
                    comment_id = %comment_id,
                    "appending a review reply failed",
                );
                format!("{error:#}")
            })?
        };
        self.note_review_activity(caller, project, branch, comment_id, true);
        Ok(status)
    }

    /// Mark `comment_id` on `(project, branch)` Resolved and record the
    /// resolve in `caller`'s turn activity buffer. `Err` when the store is
    /// closed, the write fails, or no comment with that id exists in this
    /// scope.
    pub fn review_resolve(
        &self,
        caller: &SessionKey,
        project: &str,
        branch: &str,
        comment_id: &str,
    ) -> Result<(), String> {
        {
            let guard = self.db.lock();
            let db = guard.as_ref().ok_or_else(|| "review store is unavailable".to_owned())?;
            match crate::store::review::set_status(
                db,
                project,
                branch,
                comment_id,
                ReviewStatus::Resolved,
            ) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(format!("no review comment {comment_id} on ({project}, {branch})"));
                }
                Err(error) => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        %error,
                        project = %project,
                        branch = %branch,
                        comment_id = %comment_id,
                        "resolving a review comment failed",
                    );
                    return Err(format!("{error:#}"));
                }
            }
        }
        self.note_review_activity(caller, project, branch, comment_id, false);
        Ok(())
    }

    /// Append one review action to `caller`'s turn buffer, resolving the
    /// review the action answers - the latest round the comment has a turn
    /// in. An unfiled comment is skipped: there's no review to notify about.
    fn note_review_activity(
        &self,
        caller: &SessionKey,
        project: &str,
        branch: &str,
        comment_id: &str,
        replied: bool,
    ) {
        let review_id = {
            let guard = self.db.lock();
            let Some(db) = guard.as_ref() else { return };
            match crate::store::review::find_thread_by_id(db, project, branch, comment_id) {
                Ok(Some(thread)) => thread.latest_review().map(str::to_owned),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        %error,
                        "resolving a review comment's owning review failed",
                    );
                    None
                }
            }
        };
        let Some(review_id) = review_id else { return };
        self.review_activity.lock().entry(caller.clone()).or_default().push(
            crate::mcp::review::ReviewActivity {
                project: project.to_owned(),
                branch: branch.to_owned(),
                review_id,
                replied,
            },
        );
    }

    /// Drain `caller`'s accumulated review activity into one
    /// [`SessionUpdate::ReviewActivityNotice`] per touched review, routed to
    /// the review's submit origin. A review with no recorded origin is
    /// dropped (the reviewer still sees the state on `/diff` reopen) rather
    /// than mis-routed to the caller. Called at the caller's turn end so a
    /// multi-comment review turn produces a single batched tally instead of
    /// one line per reply. Empty when the caller took no review actions this
    /// turn.
    pub(crate) fn drain_review_activity(&self, caller: &SessionKey) -> Vec<SessionUpdate> {
        let touches = { self.review_activity.lock().remove(caller).unwrap_or_default() };
        if touches.is_empty() {
            return Vec::new();
        }
        // Aggregate per review, preserving first-touched order.
        let mut order: Vec<String> = Vec::new();
        let mut by_review: HashMap<String, (String, String, usize, usize)> = HashMap::new();
        for touch in touches {
            let entry = by_review.entry(touch.review_id.clone()).or_insert_with(|| {
                order.push(touch.review_id.clone());
                (touch.project, touch.branch, 0, 0)
            });
            if touch.replied {
                entry.2 += 1;
            } else {
                entry.3 += 1;
            }
        }

        // Resolve each review's number + current open count via the store
        // (each `review_list` call takes `db` briefly and releases it) and
        // build its notice message BEFORE locking `review_origin`. Never
        // hold `review_origin` across a db-locking call - `submit_review`
        // locks db-then-origin, so the opposite order here would risk an
        // AB-BA deadlock.
        let mut messages: Vec<((String, String), usize, String)> = Vec::new();
        for review_id in order {
            let Some((project, branch, replied, resolved)) = by_review.remove(&review_id) else {
                continue;
            };
            let loaded = self.load_reviews(&project, &branch).and_then(|reviews| {
                self.load_review_threads(&project, &branch).map(|threads| (reviews, threads))
            });
            let (reviews, threads) = match loaded {
                Ok(pair) => pair,
                Err(error) => {
                    // Surface the decode / IO failure rather than swallowing
                    // it into a silently-skipped notice.
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        %error,
                        project = %project,
                        branch = %branch,
                        "review-activity notice skipped: loading reviews failed",
                    );
                    continue;
                }
            };
            let rows = crate::mcp::review::summarize(&reviews, &threads);
            let Some(summary) = rows.into_iter().find(|s| s.review_id == review_id) else {
                continue;
            };
            // Branch-wide, not per-review: the reviewer's badge counts every
            // answer still owed a look on the branch they are on.
            let waiting = threads.iter().filter(|t| t.awaits_reviewer()).count();
            let message =
                crate::mcp::review::notice_message(summary.number, replied, resolved, summary.open);
            messages.push(((project, branch), waiting, message));
        }

        // Map each review to its notice target under a short `review_origin`
        // lock (no db lock held here). When no origin was recorded (e.g. the
        // review was submitted before a restart - the map is in-memory only),
        // DROP the notice rather than falling back to the caller: routing a
        // "worker addressed review #N" line into the worker's own chat is
        // worse than not firing. The reviewer still sees the state on /diff
        // reopen (it persists in the store).
        let origins = self.review_origin.lock();
        messages
            .into_iter()
            .filter_map(|(scope, waiting, message)| {
                if let Some(key) = origins.get(&scope) {
                    Some(SessionUpdate::ReviewActivityNotice {
                        key: key.clone(),
                        branch: scope.1.clone(),
                        waiting,
                        message,
                    })
                } else {
                    tracing::debug!(
                        target: "forge_workspace::workspace",
                        project = %scope.0,
                        branch = %scope.1,
                        caller = %caller.as_str(),
                        "review-activity notice dropped: no submit origin recorded",
                    );
                    None
                }
            })
            .collect()
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
        // Per-account config dirs symlink their `projects` to one shared
        // pool; canonicalize so the scan reads it once, not once each.
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
        if let Some(cached) = self.load_usage_summary(&key)
            && signature.is_some_and(|(mtime, size)| cached.mtime == mtime && cached.size == size)
        {
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

    /// Install a redb store into a test workspace so the durable-vs-
    /// ephemeral persistence path is exercisable without `Workspace::new`.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn install_db_for_test(&self, db: crate::store::Db) {
        *self.db.lock() = Some(db);
    }

    /// Match `msg` against every active subscription and dispatch one
    /// `Command::DeliverGotifyMessage` per match. A subscription matches
    /// when its `min_priority` is `None` or `<=` the message priority AND
    /// its `applications` set is empty or contains the message's app name
    /// (resolved from the appid via the app index; an unresolved appid
    /// never matches a non-empty filter). Multiple matches fan out to
    /// every subscriber.
    pub fn route_gotify_message(self: &Arc<Self>, msg: &forge_primitives::GotifyMessage) {
        let subs = self.gotify_subs.lock().clone();
        let resolved_name = self.gotify_app_name(msg.appid);
        // Matching uses the resolved name only; the notification's `app`
        // falls back to the numeric id for display, so an unresolved appid
        // can't match a filter that happens to list that number.
        let display_name = resolved_name.clone().unwrap_or_else(|| msg.appid.to_string());
        for sub in subs {
            let priority_ok = sub.min_priority.is_none_or(|floor| msg.priority >= floor);
            let app_ok = sub.applications.is_empty()
                || resolved_name.as_ref().is_some_and(|name| sub.applications.contains(name));
            if !(priority_ok && app_ok) {
                continue;
            }
            let notification = crate::mcp::gotify::types::GotifyNotification {
                app: display_name.clone(),
                title: msg.title.clone(),
                message: msg.message.clone(),
                priority: msg.priority,
            };
            if let Err(err) = self.dispatch(Command::DeliverGotifyMessage {
                project: sub.project.clone(),
                team_role: sub.team_role.clone(),
                notification,
            }) {
                tracing::warn!(
                    target: "forge_workspace::workspace",
                    project = %sub.project,
                    error = ?err,
                    "gotify DeliverGotifyMessage dispatch failed",
                );
            }
        }
    }

    /// The application NAME for `appid` via the reverse of the app index,
    /// or `None` when the id isn't known (index not yet fetched, or a new
    /// app the server added after the last refresh).
    fn gotify_app_name(&self, appid: u64) -> Option<String> {
        self.gotify_app_index
            .lock()
            .iter()
            .find(|&(_, &id)| id == appid)
            .map(|(name, _)| name.clone())
    }

    /// Start the Gotify subsystem when it's configured, has at least one
    /// active subscription, and isn't already running. Idempotent. Spawns
    /// the reconnecting stream task plus an event pump that updates
    /// `gotify_connected`, refreshes the app index on each connect, and
    /// routes matched messages. Called at boot and after a subscribe grows
    /// the active set.
    pub fn start_gotify_subsystem(self: &Arc<Self>) {
        let Some(cfg) = self.gotify_config() else { return };
        if self.gotify_subs.lock().is_empty() {
            return;
        }
        let mut guard = self.gotify_subsystem.lock();
        if guard.is_some() {
            return;
        }
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        *guard = Some(shutdown_tx);
        drop(guard);
        tokio::spawn(run_gotify_subsystem(Arc::downgrade(self), cfg, shutdown_rx));
    }

    /// Stop the Gotify subsystem once no subscriptions remain: signal the
    /// stream task to exit and mark disconnected. No-op while any
    /// subscription is active or the subsystem isn't running.
    pub fn stop_gotify_subsystem_if_idle(&self) {
        if !self.gotify_subs.lock().is_empty() {
            return;
        }
        if let Some(shutdown_tx) = self.gotify_subsystem.lock().take() {
            let _ = shutdown_tx.send(());
            *self.gotify_connected.lock() = false;
        }
    }

    /// Advance a fired cron and persist: a recurring cron records
    /// `last_fire` and moves `next_fire` to the next future slot (removed
    /// if it somehow has none); a run-once is removed. A direct state
    /// mutation - the fire's prompt delivery goes through the Command bus
    /// separately.
    pub(crate) fn advance_or_remove_cron(
        &self,
        id: &forge_primitives::CronId,
        fired_at: std::time::SystemTime,
    ) {
        self.with_crons_mut(|crons| {
            let Some(pos) = crons.iter().position(|c| &c.id == id) else { return };
            match &crons[pos].kind {
                forge_primitives::CronKind::Once(_) => {
                    crons.remove(pos);
                }
                forge_primitives::CronKind::Recurring(_) => {
                    if let Some(next) =
                        crate::mcp::cron::schedule::next_fire_after(&crons[pos].kind, fired_at)
                    {
                        crons[pos].last_fire = Some(fired_at);
                        crons[pos].next_fire = next;
                    } else {
                        // A recurring expr that parses but never matches
                        // (e.g. "0 0 30 2 *") - reachable via a hand-edited
                        // cron.toml. Don't silently drop it (hard
                        // rule #13).
                        let removed = crons.remove(pos);
                        let expr = if let forge_primitives::CronKind::Recurring(e) = &removed.kind {
                            e.as_str()
                        } else {
                            ""
                        };
                        tracing::warn!(
                            target: "forge_workspace::workspace",
                            cron_id = %removed.id,
                            project = %removed.project_name,
                            expr = %expr,
                            "recurring cron has no upcoming occurrence; removed it",
                        );
                    }
                }
            }
        });
    }

    /// Fire every cron due at `now`: deliver each prompt into its project
    /// session (spawning it if asleep) and advance/remove the entry.
    /// Delivery routes through the Command bus (a session action); the
    /// advance is a direct state write - kept separate per the cron
    /// state-vs-delivery split. `now` is injected so tests are
    /// deterministic. Also the boot catch-up: calling this once at
    /// startup fires every cron that came due while forge was down,
    /// advancing each past its missed slots (catch-up-once).
    pub fn fire_due_crons(self: &Arc<Self>, now: std::time::SystemTime) {
        use crate::spawn::CronFireOutcome;
        let snapshot = self.all_crons_snapshot();
        let due = crate::mcp::cron::schedule::due_crons(&snapshot, now);
        for id in &due {
            let Some(cron) = snapshot.iter().find(|c| &c.id == id) else { continue };
            // Overdue by more than two ticks: forge or the owner was down
            // through the scheduled minute. Two ticks (not one) absorbs the
            // scheduler's Skip-behaviour jitter so a same-window fire under
            // load is never mislabelled missed.
            let missed = now > cron.next_fire + CRON_TICK_INTERVAL * 2;
            match crate::spawn::deliver_cron_prompt(
                self,
                &cron.project_name,
                cron.team_role.as_deref(),
                cron.prompt.clone(),
                missed,
            ) {
                // Delivered (or spawn kicked off): advance a recurring to
                // its next slot, remove a fired run-once.
                CronFireOutcome::Delivered => self.advance_or_remove_cron(id, now),
                // Owner gone (project removed from forge.toml, or a worker
                // label that is no longer static or a durable dynamic row):
                // remove the cron rather than advance a dead entry forever.
                CronFireOutcome::TargetGone => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        project = %cron.project_name,
                        cron_id = %id,
                        "cron owner gone; removing the cron",
                    );
                    self.remove_cron(&cron.project_name, id);
                }
                // Command channel closed (shutting down): leave the cron
                // due so the next boot catch-up re-fires it - don't consume
                // a fire that never handed off.
                CronFireOutcome::DispatchFailed => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        project = %cron.project_name,
                        cron_id = %id,
                        "cron fire dispatch failed; leaving it due for the next boot",
                    );
                }
            }
        }
    }

    /// Spawn the cron scheduler: a background task that wakes every
    /// `CRON_TICK_INTERVAL` (~60s), fires every due cron, and exits when
    /// the workspace drops. Idempotent (a second call no-ops). Mirrors
    /// [`Workspace::start_usage_poller`]; started once at boot.
    pub fn start_cron_scheduler(self: &Arc<Self>) {
        if self.cron_scheduler_started.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("cron_scheduler");
        tokio::spawn(
            async move {
                let mut interval = tokio::time::interval_at(
                    tokio::time::Instant::now() + CRON_TICK_INTERVAL,
                    CRON_TICK_INTERVAL,
                );
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(workspace) = weak.upgrade() else {
                        return;
                    };
                    workspace.fire_due_crons(std::time::SystemTime::now());
                }
            }
            .instrument(span),
        );
    }

    /// Snapshot every live worker's session_key across every project.
    /// Used by the TUI's `find_running_bucket_for_path` to exclude
    /// worker buckets from project-row click routing without depending
    /// on `list_projects()` for enumeration.
    pub fn all_live_worker_session_keys(&self) -> Vec<SessionKey> {
        self.live_workers
            .lock()
            .values()
            .flat_map(|entries| entries.iter().map(|e| e.session_key.clone()))
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
    /// its label in `project_key`. Holds `live_workers.lock()` across the
    /// label-check AND the push, so two genuinely-concurrent SpawnWorker
    /// dispatches for the same label (a reconnect re-spawn racing a manual
    /// `workers__spawn`, say) can't both pass a check-then-insert window
    /// and fork two subprocesses onto one worktree. Returns `Ok(())` on
    /// insert, or `Err(session_key)` naming the live worker that already
    /// holds the label. This is the sole enforcement point for the
    /// at-most-one-live-worker-per-label invariant.
    pub fn insert_live_worker_if_label_absent(
        &self,
        project_key: &ProjectKey,
        entry: crate::mcp::workers::types::WorkerEntry,
    ) -> Result<(), SessionKey> {
        let mut workers = self.live_workers.lock();
        let entries = workers.entry(project_key.clone()).or_default();
        if let Some(existing) =
            crate::mcp::workers::types::live_worker_with_label(entries, &entry.label)
        {
            return Err(existing.session_key.clone());
        }
        entries.push(entry);
        Ok(())
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
        session_key: &SessionKey,
    ) -> Option<(ProjectKey, crate::mcp::workers::types::WorkerEntry)> {
        let mut map = self.live_workers.lock();
        for (project_key, entries) in map.iter_mut() {
            if let Some(idx) = entries.iter().position(|e| e.session_key == *session_key) {
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

    /// Locate `(project_key, label, is_git_repo_at_spawn)` for any
    /// worker matching `session_key` across every project's
    /// `live_workers`. Used by the Connected handler in
    /// `SessionTask::translate_event` to decide whether a just-
    /// connected session is a worker (and what tag to write).
    /// `is_git_repo_at_spawn` lets the tag-write path route to the
    /// worktree-derived JSONL via `worker_tag_dir` in
    /// `crate::mcp::workers::types`.
    /// `None` when the session is a lead (or not a worker at all).
    pub fn worker_lookup_for_session(
        &self,
        session_key: &SessionKey,
    ) -> Option<(ProjectKey, String, bool)> {
        let workers = self.live_workers.lock();
        for (project_key, entries) in workers.iter() {
            if let Some(entry) = entries.iter().find(|e| e.session_key == *session_key) {
                return Some((
                    project_key.clone(),
                    entry.label.clone(),
                    entry.is_git_repo_at_spawn,
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
    /// [`Self::resume_cwd_for_session`] hands claude an empty cwd,
    /// while the review MCP reports `SessionCwdUnknown` to the caller.
    ///
    /// [`worker_tag_dir`]: crate::mcp::workers::types::worker_tag_dir
    pub(crate) fn cwd_for_session(&self, session_key: &SessionKey) -> Option<String> {
        if let Some(cwd) = self.session_cwd_for(session_key) {
            return Some(cwd);
        }
        let (project_key, label, is_git) = self.worker_lookup_for_session(session_key)?;
        let Some(root) = self.project_root_for_key(&project_key) else {
            // Unreachable while `forge.toml` and `live_workers` agree,
            // so treat a firing as drift rather than a normal miss.
            tracing::warn!(
                target: "forge_workspace::workspace",
                event_name = "session_cwd_registry_contradiction",
                session_key = %session_key.as_str(),
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
    pub(crate) fn resume_cwd_for_session(&self, session_key: &SessionKey) -> String {
        self.cwd_for_session(session_key).unwrap_or_else(|| {
            tracing::warn!(
                target: "forge_workspace::workspace",
                session_key = %session_key.as_str(),
                "resume_cwd_for_session: no catalog cwd and no live worker entry; \
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
    /// [`Self::resume_cwd_for_session`] to derive the project root
    /// from a worker's project_key without depending on the worker's
    /// `cwd_raw` value (which carries the project root for fresh
    /// spawns and the worktree path for resumed sessions - the two
    /// cases would otherwise need different composition logic).
    pub(crate) fn project_root_for_key(&self, target: &ProjectKey) -> Option<std::path::PathBuf> {
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
                return Some(project.path);
            }
        }
        self.config.projects.iter().find(|p| &derive_key(p) == target).map(|p| p.path.clone())
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
        session_key: &SessionKey,
        cwd_raw: &std::path::Path,
    ) -> std::path::PathBuf {
        let Some((project_key, label, is_git_repo_at_spawn)) =
            self.worker_lookup_for_session(session_key)
        else {
            // Trace-level so a real lookup-miss (race during
            // worker spawn, lead-session call) leaves a grep-able
            // trail without flooding normal logs. The lead-session
            // case is the common path and intentionally not
            // promoted higher.
            tracing::trace!(
                target: "forge_workspace::git_scan",
                session_key = %session_key.as_str(),
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
                session_key = %session_key.as_str(),
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
        session_key: &SessionKey,
        message: &str,
    ) -> bool {
        // Look up the worker entry WITHOUT removing it yet - the
        // worktree-failure path still removes (rollback semantics
        // for "worker never existed"); the general-failure path
        // transitions to Failed so the user sees what happened.
        let lookup = {
            let workers = self.live_workers.lock();
            workers.iter().find_map(|(project_key, entries)| {
                entries
                    .iter()
                    .find(|e| e.session_key == *session_key)
                    .map(|entry| (project_key.clone(), entry.clone()))
            })
        };
        let Some((project_key, entry)) = lookup else {
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
        );
        if let crate::mcp::workers::facade::WorkerSpawnError::WorktreeCreationFailed { reason } =
            &classified
        {
            // Worktree-creation failure: roll back the worker entry
            // (the worker never existed; the user-visible signal is
            // the typed notice routed to the lead, not the worker row).
            self.remove_worker_by_session_key(session_key);
            // A dynamic worker persisted its row on the optimistic spawn
            // reply, before this async failure. A worktree-creation
            // failure is a hard removal (the worker never started), so
            // delete the row too - otherwise it zombie-re-spawns every
            // restart despite a visibly-failed spawn. No-op for static
            // workers (no row). The transition-to-Failed path below
            // deliberately keeps the row: a Failed-but-visible worker
            // wasn't despawned, so it should re-spawn to recover or
            // re-fail visibly.
            self.delete_dynamic_worker(&project_key, &entry.label);
            // Notice goes to the lead session that spawned this
            // worker. Use the workspace's update channel + a fresh
            // Command::Prompt so the lead's claude subprocess sees
            // the envelope as a user turn and the TUI render path
            // picks up the bracketed prefix via peer_block::detect_inbound.
            let lead_session_id = entry.spawned_by_session_id.clone();
            let lead_key = SessionKey::from_session_id(lead_session_id.clone());
            let pool_has_lead = self.pool.lock().contains_key(&lead_key);
            if pool_has_lead {
                let wrapped = WrappedPrompt {
                    correlation_id: CorrelationId::new_tell(),
                    kind: WrappedKind::WorkerSpawnFailedNotice,
                    channel: AskChannel::Workers,
                    sender_name: entry.label.clone(),
                    sender_org: String::new(),
                    body: reason.clone(),
                };
                if let Err(err) = self.dispatch(Command::Prompt {
                    key: lead_key,
                    text: wrapped.to_prose(),
                    attachments: Vec::new(),
                }) {
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
                    lead_session_id = %lead_session_id,
                    "worker spawn failed but lead session is gone; dropping notice",
                );
            }
            // Emit WorkerStatusChanged::Removed - parity with the
            // sync rollback in handle_spawn_worker.
            let _ = self.update_tx.send(SessionUpdate::WorkerStatusChanged {
                project_key,
                action: crate::protocol::WorkerStatusAction::Removed,
                status: entry.to_status(),
                is_git_repo_at_spawn: entry.is_git_repo_at_spawn,
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
            transition_worker_to_failed(self, &project_key, session_key, diagnostic);
        }
        true
    }

    /// Tag a worker session's JSONL with `forge:worker:<label>` and
    /// transition its `WorkerEntry` from `Spawning` to `Running`.
    /// Called from `SessionTask::translate_event` immediately after the
    /// first `Connected` rekey lands.
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
        session_key: &SessionKey,
        cwd: &str,
    ) {
        let Some((project_key, label, is_git_repo_at_spawn)) =
            self.worker_lookup_for_session(session_key)
        else {
            return;
        };
        // Resolve the per-account config_dir for this session via the
        // bridge so the tag-write lands under the right account's
        // projects/ tree.
        let Some(config_dir) = self.config_dir_for(session_key) else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                session_id = %session_key.as_str(),
                "apply_worker_tag: no agent registered; cannot resolve config_dir"
            );
            return;
        };
        self.apply_worker_tag_or_rollback_with_config_dir(
            session_key,
            &project_key,
            &label,
            cwd,
            is_git_repo_at_spawn,
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
    pub(crate) fn apply_worker_tag_or_rollback_with_config_dir(
        self: &Arc<Self>,
        session_key: &SessionKey,
        project_key: &ProjectKey,
        label: &str,
        cwd: &str,
        is_git_repo_at_spawn: bool,
        config_dir: &std::path::Path,
    ) {
        use tracing::Instrument;
        let workspace = Arc::clone(self);
        let project_key = project_key.clone();
        let session_key = session_key.clone();
        let label = label.to_owned();
        let effective_cwd = crate::mcp::workers::types::worker_tag_dir(
            std::path::Path::new(cwd),
            &label,
            is_git_repo_at_spawn,
        );
        let config_dir = config_dir.to_path_buf();
        let span = tracing::info_span!(
            "forge_workspace::worker_tag_write",
            session_id = %session_key.as_str(),
            label = %label,
        );
        tokio::spawn(async move {
            let tag = forge_primitives::worker_tag(&label);
            let result = tag_session_with_retry(
                &config_dir,
                session_key.as_str(),
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
                        session_id = %session_key.as_str(),
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
                        session_id = %session_key.as_str(),
                        label = %label,
                        error = ?err,
                        "tag_session failed for worker (non-NotFound); rolling back spawn"
                    );
                    let removed = workspace.remove_latest_worker(&project_key, &label);
                    if let Some(entry) = removed {
                        let status = entry.to_status();
                        let is_git_repo_at_spawn = entry.is_git_repo_at_spawn;
                        let _ = workspace.update_tx.send(SessionUpdate::WorkerStatusChanged {
                            project_key,
                            action: crate::protocol::WorkerStatusAction::Removed,
                            status,
                            is_git_repo_at_spawn,
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
        session_key: &SessionKey,
        label: &str,
        cwd: &str,
        is_git_repo_at_spawn: bool,
    ) {
        let Some(config_dir) = self.config_dir_for(session_key) else {
            tracing::warn!(
                target: "forge_workspace::workspace",
                session_id = %session_key.as_str(),
                "retry_worker_tag: no agent registered; cannot resolve config_dir"
            );
            return;
        };
        self.retry_worker_tag_opportunistic_with_config_dir(
            project_key,
            session_key,
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
        session_key: &SessionKey,
        label: &str,
        cwd: &str,
        is_git_repo_at_spawn: bool,
        config_dir: &std::path::Path,
    ) {
        use tracing::Instrument;
        let workspace = Arc::clone(self);
        let project_key = project_key.clone();
        let session_key = session_key.clone();
        let label = label.to_owned();
        let effective_cwd = crate::mcp::workers::types::worker_tag_dir(
            std::path::Path::new(cwd),
            &label,
            is_git_repo_at_spawn,
        );
        let config_dir = config_dir.to_path_buf();
        let span = tracing::info_span!(
            "forge_workspace::worker_tag_write",
            session_id = %session_key.as_str(),
            label = %label,
        );
        tokio::spawn(async move {
            let tag = forge_primitives::worker_tag(&label);
            let result = tag_session_with_retry(
                &config_dir,
                session_key.as_str(),
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
                            entries.iter_mut().find(|e| e.session_key == session_key).map(|entry| {
                                entry.needs_tag = false;
                                (entry.to_status(), entry.is_git_repo_at_spawn)
                            })
                        })
                    };
                    if let Some((status, is_git_repo_at_spawn)) = updated {
                        tracing::info!(
                            target: "forge_workspace::workspace",
                            session_id = %session_key.as_str(),
                            label = %label,
                            "tag_session_retry: deferred tag-write succeeded on opportunistic retry"
                        );
                        let _ = workspace.update_tx.send(SessionUpdate::WorkerStatusChanged {
                            project_key,
                            action: crate::protocol::WorkerStatusAction::StatusChanged,
                            status,
                            is_git_repo_at_spawn,
                        });
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        target: "forge_workspace::workspace",
                        session_id = %session_key.as_str(),
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
    pub fn refresh_status_snapshot(&self, key: &SessionKey) -> Result<(), DispatchError> {
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
        key: &SessionKey,
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
    pub fn refresh_context_usage(&self, key: &SessionKey) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.get_context_usage(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Reload session plugins for `key`. The bridge replies via
    /// [`SessionUpdate::RuntimeReloadCompleted`] / `RuntimeReloadFailed`.
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn reload_plugins(&self, key: &SessionKey) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.reload_plugins(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    /// Request a fresh MCP server snapshot for `key`. The bridge
    /// replies via [`SessionUpdate::McpSnapshot`].
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_status_snapshot`].
    pub fn refresh_mcp_snapshot(&self, key: &SessionKey) -> Result<(), DispatchError> {
        let (handle, sid) = self.handle_and_session_id(key)?;
        handle.get_mcp_snapshot(sid).map_err(|_| DispatchError::SessionClosed(key.clone()))
    }

    // ---- Direct-accessor facades (workspace owns the bridge call) ----

    /// Resolve the auto-memory path the bridge would consult for
    /// `cwd`, scoped to `key`'s configured account. Returns `None`
    /// when no agent is registered for `key`.
    pub fn project_memory_path(&self, key: &SessionKey, cwd: &std::path::Path) -> Option<PathBuf> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.project_memory_path(cwd))
    }

    /// Snapshot the bridge's settings documents for `key` at `cwd`.
    /// Returns `None` when no agent is registered for `key`.
    pub fn settings_documents(
        &self,
        key: &SessionKey,
        cwd: &std::path::Path,
    ) -> Option<forge_agent::userdata::settings::SettingsDocuments> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.settings_documents(cwd))
    }

    /// Resolve the agent's configured config_dir for `key`. Returns
    /// `None` when no agent is registered for `key`.
    pub fn config_dir_for(&self, key: &SessionKey) -> Option<PathBuf> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.config_dir())
    }

    /// OS PID of the `claude` subprocess bound to `key`. Returns
    /// `None` when the session has no live client (pre-spawn /
    /// post-disconnect / synthetic spawn bucket). The PID is stable
    /// for the lifetime of the subprocess, so consumers (e.g. the
    /// Inspector pane's PROCESSES OS walk) can cache snapshots
    /// keyed off this value.
    pub fn claude_pid(&self, key: &SessionKey) -> Option<u32> {
        self.agent_handle_for(key).and_then(|handle| handle.claude_pid())
    }

    /// Borrow the [`Arc<AgentHandle>`] registered against `key`.
    /// Workspace-internal helper - surfaces a sometimes-`None` to keep
    /// the early-init / disconnected branches explicit.
    fn agent_handle_for(&self, key: &SessionKey) -> Option<Arc<AgentHandle>> {
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
        key: &SessionKey,
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

/// Map an [`OauthUsageError`] to the renderer-facing
/// [`account::UsageFetchStatus`] bucket. Separates HTTP 429 (the
/// common multi-instance throttle case) from the auth-related
/// failures (`Expired` / `NoCredentials` / `Unauthorized`) and
/// transport failures (`Network`), so the TUI's bottom-panel hint
/// can tell the user something specific rather than a generic
/// "fetch error".
pub(crate) fn classify_oauth_usage_error(
    err: &forge_primitives::usage::oauth::OauthUsageError,
) -> account::UsageFetchStatus {
    use account::UsageFetchStatus;
    use forge_primitives::usage::oauth::OauthUsageError;
    match err {
        OauthUsageError::RateLimited { .. } | OauthUsageError::HttpStatus(429, _) => {
            UsageFetchStatus::RateLimited
        }
        OauthUsageError::Unauthorized(_) => UsageFetchStatus::Unauthorized,
        OauthUsageError::NoCredentials | OauthUsageError::Expired => UsageFetchStatus::Expired,
        OauthUsageError::Network(_) => UsageFetchStatus::NetworkFailed,
        OauthUsageError::HttpStatus(_, _) | OauthUsageError::Decode(_) => UsageFetchStatus::Other,
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
        key: SessionKey,
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
                key = %key.as_str(),
                "register_domain_session overwriting existing entry - pending interactions lost"
            );
        }
        handles.insert(key, Arc::clone(&domain));
        domain
    }

    /// Migrate an existing `DomainSession` registration from `from` to
    /// `to`. Used by the TUI's `set_session_id` migration when the
    /// pre-Connect synthetic key gets replaced with the real
    /// claude-issued session id. No-op when `from` is not registered.
    pub fn rekey_domain_session(&self, from: &SessionKey, to: SessionKey) {
        let mut handles = self.domain_handles.lock();
        if let Some(domain) = handles.remove(from) {
            handles.insert(to, domain);
        }
    }

    /// Migrate this workspace's per-`SessionTask` registrations from
    /// `from` to `to`. Called by the per-session task actor on
    /// `Connected` / `SessionReplaced` when the pool key it was
    /// registered under differs from the real claude-issued session
    /// UUID - i.e., the `/new` / `/resume` / first-Connect-of-a-fresh-
    /// project paths where the pool key was a placeholder (a previous
    /// session's id or `__fresh__:<project_key>`) and the actual
    /// session UUID isn't known until the bridge fires `init`.
    ///
    /// Atomically moves the entries in `pool`, `command_senders`, and
    /// `domain_handles` (and rewrites the moved `DomainSession.key`
    /// field). Without this migration, `Workspace::dispatch`'s key
    /// lookup falls off the end with `UnknownSession` for every
    /// `Command::Prompt` / `Cancel` / etc. after a session-replace -
    /// the SessionTask is still alive at the old key but the TUI's
    /// `active_session_key` has flipped to the new one.
    ///
    /// No-op when `from == to` or when `from` is not registered.
    pub fn migrate_session_task(&self, from: &SessionKey, to: &SessionKey) -> bool {
        if from == to {
            return true;
        }
        // Lock order matches `get_agent_handle`'s insertion order:
        // pool → command_senders → domain_handles.
        let mut pool = self.pool.lock();
        let mut senders = self.command_senders.lock();
        let mut handles = self.domain_handles.lock();
        // Refuse the migration if `to` is already registered - moving
        // would silently replace a live SessionTask's entries and
        // orphan its pending interactions. This is a hint that a
        // duplicate Connected event arrived for two SessionTasks
        // pointing at the same target.
        if pool.contains_key(to) || senders.contains_key(to) || handles.contains_key(to) {
            tracing::error!(
                target: "forge_workspace::workspace",
                from = %from.as_str(),
                to = %to.as_str(),
                "migrate_session_task: target key already registered; migration skipped"
            );
            return false;
        }
        if let Some(pooled) = pool.remove(from) {
            pool.insert(to.clone(), pooled);
        }
        if let Some(sender) = senders.remove(from) {
            senders.insert(to.clone(), sender);
        }
        if let Some(domain) = handles.remove(from) {
            domain.lock().key = to.clone();
            handles.insert(to.clone(), domain);
        }
        drop((pool, senders, handles));
        // Worker entries are keyed by `session_key` which initially
        // matches the synthetic spawn key; rekey here so subsequent
        // lookups (close-worker by label, deliver-worker-prompt) find
        // the worker under its real claude-issued UUID.
        // `spawned_by_session_id` stays at its spawn-time value
        // (historical record); current-lead lookups now go through
        // `caller_context::caller_context` and don't depend on this
        // field being fresh (see #298 Cause 2).
        {
            let mut workers = self.live_workers.lock();
            for entries in workers.values_mut() {
                for entry in entries.iter_mut() {
                    if entry.session_key == *from {
                        entry.session_key = to.clone();
                    }
                }
            }
        }
        // Peer badges + open-ask keys follow the session across the
        // rekey. peer_stats MERGES into any counts already at `to`
        // (never clobbers or drops - erring toward keeping counts);
        // open asks keyed on `from` (as caller or stamped target) are
        // rewritten so replies + expiry hit the live key.
        {
            let mut stats = self.peer_stats.lock();
            if let Some(from_stats) = stats.remove(from) {
                let entry = stats.entry(to.clone()).or_default();
                entry.outgoing = entry.outgoing.saturating_add(from_stats.outgoing);
                entry.incoming = entry.incoming.saturating_add(from_stats.incoming);
                entry.delivery_failed =
                    entry.delivery_failed.saturating_add(from_stats.delivery_failed);
            }
        }
        {
            let mut asks = self.inflight_asks.lock();
            for ask in asks.values_mut() {
                if ask.caller == *from {
                    ask.caller = to.clone();
                }
                if ask.target_session.as_ref() == Some(from) {
                    ask.target_session = Some(to.clone());
                }
            }
        }
        true
    }

    /// Stamp the session that received an ask's `IncomingPlus1` onto
    /// its `InflightAsk`, paired with every Question delivery so a
    /// later `expire_inflight_ask_failed` can clear that session's
    /// incoming badge (no-op once the ask completes).
    pub(crate) fn stamp_inflight_target(&self, id: &CorrelationId, target: &SessionKey) {
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
        closing_key: &SessionKey,
        reason: crate::mcp::peers::types::PeerFailureReason,
    ) {
        // Find the project this closing session belongs to. Workers
        // never enter the catalog mirror, so this lookup can miss for
        // them; the target_session predicate below still catches
        // their delivered asks.
        let project_name = self
            .list_projects()
            .into_iter()
            .find(|v| v.sessions.iter().any(|s| s.session == *closing_key))
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
        if let Err(err) = self.dispatch(crate::protocol::Command::Prompt {
            key: ask.caller.clone(),
            text: caller_notice.to_prose(),
            attachments: Vec::new(),
        }) {
            tracing::warn!(
                target: "forge_workspace::workspace",
                correlation_id = %id,
                error = ?err,
                "expire_inflight_ask_failed: caller notice dispatch failed (caller closed?)"
            );
        }
    }

    /// Deliver a Reply straight to the asker's session, bypassing
    /// name/label resolution. The asker is identified by `SessionKey`
    /// (a worker asker has no addressable project name), so this
    /// by-session path is load-bearing for closing a cross-agent ask.
    /// Confirms the caller session is still live before dispatching.
    /// Shared by the peers + workers facades.
    pub(crate) fn deliver_reply_to_caller(
        self: &Arc<Self>,
        caller: &SessionKey,
        reply: &WrappedPrompt,
    ) -> Result<(), crate::mcp::peers::facade::ReplyDeliverError> {
        use crate::mcp::peers::facade::ReplyDeliverError;
        if !self.pool.lock().contains_key(caller) {
            return Err(ReplyDeliverError::CallerSessionGone);
        }
        // The CLI never echoes stdin-injected prompts back, so paint the
        // visible reply block ourselves before the LLM-side dispatch.
        crate::spawn::push_peer_user_turn_into_chat(self, caller, reply);
        if let Err(err) = self.dispatch(crate::protocol::Command::Prompt {
            key: caller.clone(),
            text: reply.to_prose(),
            attachments: Vec::new(),
        }) {
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

    /// Fail every peer ask buffered against a project-spawn synth key
    /// that never connected. The sleeping-target delivery path parks
    /// wrapped prompts on the `__spawn_<project>__` `DomainSession`
    /// and kicks a spawn; if that spawn never reaches `Connected`, the
    /// buffered asks were never delivered (no `target_session` stamp)
    /// and the synth key resolves to no catalog project, so
    /// `expire_target_inflight` can't reach them. Drain and fail each
    /// so the caller's LLM gets its `DeliveryFailureNotice`. No-op for
    /// a key with no buffered prompts (any non-synth or already-drained
    /// session).
    pub(crate) fn expire_buffered_peer_prompts(
        self: &Arc<Self>,
        synth_key: &SessionKey,
        reason: crate::mcp::peers::types::PeerFailureReason,
    ) {
        let buffered = {
            let domain = self.domain_handles.lock().get(synth_key).cloned();
            let Some(domain) = domain else {
                return;
            };
            std::mem::take(&mut domain.lock().pending_peer_prompts)
        };
        for wrapped in buffered {
            self.expire_inflight_ask_failed(&wrapped.correlation_id, reason);
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Mark a session as having completed its Connected handshake by
    /// stamping `session_id` on its `DomainSession` (registering one if
    /// absent). Delivery paths gate dispatch-vs-buffer on this, so tests
    /// that assert a live worker/lead receives a prompt need it set -
    /// in production every Running session has it stamped by `Connected`.
    #[cfg(test)]
    pub(crate) fn mark_session_connected_for_test(&self, key: &SessionKey, session_id: &str) {
        let domain = self
            .domain_session_for(key)
            .unwrap_or_else(|| self.register_domain_session(key.clone(), None));
        domain.lock().session_id = Some(forge_primitives::SessionId::new(session_id));
    }

    /// Construct a stub `AgentHandle` plus the matching
    /// `Receiver<forge_primitives::AgentCommand>` that drains every command
    /// dispatched to it. Tests use this to wire `App.set_active_conn`
    /// without spinning up a real subprocess; the bridge underneath is
    /// `forge_agent::Agent::testing_stub` - same shape as before, now
    /// reachable from forge-tui via `forge_workspace::Workspace::*`
    /// so the TUI crate no longer needs a direct `forge-agent` dep.
    ///
    /// The returned `Receiver` carries `forge_primitives::AgentCommand`
    /// because that's what the bridge's dispatcher accepts; this is
    /// distinct from [`crate::protocol::Command`] (the workspace's
    /// outer envelope) that wraps these primitives under a
    /// `SessionKey`.
    pub fn testing_stub_handle()
    -> (forge_agent::AgentHandle, mpsc::UnboundedReceiver<forge_primitives::AgentCommand>) {
        forge_agent::Agent::testing_stub()
    }

    /// Register a fresh testing-stub agent against `key`'s
    /// `DomainSession`. Returns the matching
    /// `forge_primitives::AgentCommand` receiver so tests can assert on
    /// the commands the workspace routes through it (the same shape
    /// as `testing_stub_handle()`, but with the handle installed in
    /// one step so TUI test code doesn't have to touch
    /// `AgentHandle` directly).
    ///
    /// Auto-creates a `DomainSession` for `key` when none is
    /// registered yet; otherwise overwrites the existing
    /// `DomainSession.conn` slot.
    pub fn install_testing_stub(
        &self,
        key: &SessionKey,
    ) -> mpsc::UnboundedReceiver<forge_primitives::AgentCommand> {
        let (handle, rx) = forge_agent::Agent::testing_stub();
        let arc = Arc::new(handle);
        let domain = self
            .domain_session_for(key)
            .unwrap_or_else(|| self.register_domain_session(key.clone(), None));
        domain.lock().conn = Some(arc);
        rx
    }

    /// Construct an empty `Workspace` for use in unit tests. Skips
    /// the on-disk `forge.toml` load + catalog scan that
    /// [`Workspace::new`] performs; the returned workspace carries an
    /// empty project list and an empty pool. Tests register a domain
    /// session via [`Self::register_domain_session`] before
    /// exercising any code path that needs one.
    ///
    /// Returns the workspace alongside the `SessionUpdate` receiver.
    /// The workspace's `subscribe()` slot is `None` - callers that
    /// need the receiver get it directly from this constructor.
    pub fn testing_stub() -> (Arc<Self>, mpsc::UnboundedReceiver<SessionUpdate>) {
        Self::testing_stub_with_config_dir(PathBuf::from("/tmp/forge-testing-stub"))
    }

    /// Like `testing_stub` but with a caller-supplied `config_dir` so
    /// tests that probe disk-side APIs (catalog scan, JSONL reads)
    /// can point the stub at a tempdir of their own.
    pub fn testing_stub_with_config_dir(
        config_dir: PathBuf,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<SessionUpdate>) {
        Self::testing_stub_with_config(config_dir, LoadedConfig::empty_for_test())
    }

    /// Like `testing_stub_with_config_dir` but injects a caller-built
    /// `LoadedConfig` so tests can drive the project-resolution paths
    /// (`project_accounts_for` / `default_project`) that read
    /// `self.config.projects`, which the `test_extra_projects` overlay
    /// does not populate. Build the config via
    /// `crate::config::load_from_dir` on a tempdir `forge.toml` fixture;
    /// `db` stays `None`, so nothing touches the real machine store.
    pub(crate) fn testing_stub_with_config(
        config_dir: PathBuf,
        config: LoadedConfig,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<SessionUpdate>) {
        // Mirror the boot-time `ensure_forge_data_dir`: stub-based tests
        // that exercise the cron / state stores expect `forge/` present.
        let _ = crate::config::ensure_forge_data_dir(&config_dir);
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let (kick_dispatcher_tx, kick_dispatcher_rx) = mpsc::unbounded_channel::<KickRequest>();
        let workspace = Self {
            config_dir,
            config,
            catalog: Mutex::new(HashMap::new()),
            pool: Mutex::new(HashMap::new()),
            accounts: Mutex::new(AccountStateMap::empty_for_test()),
            assignment_plan: Mutex::new(None),
            update_tx,
            update_rx_slot: Mutex::new(None),
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
            // testing_stub skips Workspace::new and therefore the
            // proxy boot. Tests don't drive real subprocesses.
            proxy: None,
            _single_instance_lock: None,
            crons: Mutex::new(Vec::new()),
            pending_cron_by_owner: Mutex::new(HashMap::new()),
            gotify_subs: Mutex::new(Vec::new()),
            db: Mutex::new(None),
            gotify_connected: Mutex::new(false),
            gotify_app_index: Mutex::new(HashMap::new()),
            gotify_subsystem: Mutex::new(None),
            team_spawn_in_flight: Mutex::new(std::collections::HashSet::new()),
            command_intercept: Mutex::new(None),
            test_extra_projects: Mutex::new(Vec::new()),
        };
        (Arc::new(workspace), update_rx)
    }
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Enable test-mode app-level command interception. After this
    /// call, every `Command` routed through the app-level branch of
    /// `dispatch` is buffered in lieu of running the spawn handler;
    /// drain via `drain_test_dispatch_buffer`. No-op if already
    /// enabled. Test-only - tests use this to assert what would
    /// have been dispatched without spinning up real subprocesses.
    pub fn enable_test_dispatch_intercept(&self) {
        let mut intercept = self.command_intercept.lock();
        if intercept.is_none() {
            *intercept = Some(Vec::new());
        }
    }

    /// Drain every app-level `Command` captured since the last call.
    /// Returns empty when no intercept was enabled or no commands
    /// were dispatched. Test-only.
    pub fn drain_test_dispatch_buffer(&self) -> Vec<crate::protocol::Command> {
        let mut intercept = self.command_intercept.lock();
        match intercept.as_mut() {
            Some(buffer) => std::mem::take(buffer),
            None => Vec::new(),
        }
    }

    /// Append a synthetic project to the test overlay searched first
    /// by `find_project_view_by_name`. Used by engineering-team tests
    /// to drive the Connected-hook worker-spawn trigger without
    /// writing a real `forge.toml`. Test-only.
    pub fn seed_test_project_with_static_workers(
        &self,
        name: &str,
        path: &str,
        static_workers: &[String],
    ) {
        self.test_extra_projects.lock().push(crate::config::LoadedProject {
            name: name.to_owned(),
            path: std::path::PathBuf::from(path),
            display_path: path.to_owned(),
            org: "TestOrg".to_owned(),
            accounts: vec!["acct-a".to_owned()],
            auto_start: false,
            static_workers: static_workers.to_vec(),
            env: std::collections::HashMap::new(),
        });
    }

    /// Register a durable cron directly, bypassing the MCP create path.
    /// Cross-crate test access to the otherwise `pub(crate)` cron store
    /// so forge-tui can exercise the Inspector's `refresh_forge_crons`
    /// resolution against a seeded cron.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_test_cron(&self, entry: forge_primitives::CronEntry) {
        self.push_cron(entry);
    }

    /// Register a Gotify subscription directly, bypassing the MCP
    /// subscribe path. Cross-crate test access so forge-tui can exercise
    /// the Inspector's `refresh_gotify` resolution, mirroring
    /// [`Self::seed_test_cron`].
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_test_gotify_subscription(&self, sub: forge_primitives::GotifySubscription) {
        self.add_gotify_subscription(sub, false);
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
    session_key: &SessionKey,
    result: TagWriteResult,
) {
    let updated = {
        let mut workers = workspace.live_workers.lock();
        workers.get_mut(project_key).and_then(|entries| {
            entries.iter_mut().find(|e| e.session_key == *session_key).map(|entry| {
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
            is_git_repo_at_spawn,
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
    session_key: &SessionKey,
    diagnostic: Option<String>,
) {
    let updated = {
        let mut workers = workspace.live_workers.lock();
        workers.get_mut(project_key).and_then(|entries| {
            entries.iter_mut().find(|e| e.session_key == *session_key).and_then(|entry| {
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
            session = %session_key.as_str(),
            diagnostic = ?status.diagnostic,
            "worker session transitioned to Failed; row will render with diagnostic sub-line",
        );
        let _ = workspace.update_tx.send(SessionUpdate::WorkerStatusChanged {
            project_key: project_key.clone(),
            action: crate::protocol::WorkerStatusAction::StatusChanged,
            status,
            is_git_repo_at_spawn,
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

/// Test helper: ensure `forge/` exists and return the production
/// `forge/forge.toml` path, so tests write where forge reads (not the
/// legacy top-level fallback).
#[cfg(test)]
fn forge_toml_path(config_dir: &std::path::Path) -> PathBuf {
    crate::config::ensure_forge_data_dir(config_dir).expect("forge/ dir").join("forge.toml")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod resolver_tests {
    use super::*;

    fn mk_session(id: &str, tag: Option<&str>, mtime: u64) -> SDKSessionInfo {
        SDKSessionInfo {
            session_id: id.to_owned(),
            summary: format!("session {id}"),
            last_modified: mtime,
            file_size: None,
            custom_title: None,
            first_prompt: None,
            git_branch: None,
            cwd: None,
            storage_key: String::new(),
            tag: tag.map(str::to_owned),
            created_at: None,
        }
    }

    #[test]
    fn resolver_prefers_forge_lead_over_untagged() {
        // Untagged session is newer, but the lead tag wins regardless.
        let sessions = vec![
            mk_session("untagged-newer", None, 200),
            mk_session("tagged-older", Some(forge_primitives::FORGE_LEAD_TAG), 100),
        ];
        let picked = resolve_lead_session(&sessions).expect("some");
        assert_eq!(picked.session_id, "tagged-older");
    }

    #[test]
    fn resolver_falls_back_to_untagged_when_no_lead_tag() {
        let sessions = vec![mk_session("legacy", None, 100)];
        let picked = resolve_lead_session(&sessions).expect("some");
        assert_eq!(picked.session_id, "legacy");
    }

    #[test]
    fn resolver_skips_forge_worker_tagged_sessions() {
        // Worker tag must never win, even when it's the only candidate
        // and its mtime is overwhelmingly newer than anything else.
        let worker = forge_primitives::worker_tag("reviewer");
        let sessions = vec![mk_session("worker", Some(&worker), 999)];
        assert!(resolve_lead_session(&sessions).is_none());
    }

    #[test]
    fn resolver_picks_latest_lead_when_multiple() {
        let sessions = vec![
            mk_session("old-lead", Some(forge_primitives::FORGE_LEAD_TAG), 100),
            mk_session("new-lead", Some(forge_primitives::FORGE_LEAD_TAG), 200),
        ];
        let picked = resolve_lead_session(&sessions).expect("some");
        assert_eq!(picked.session_id, "new-lead");
    }

    #[test]
    fn resolver_picks_latest_untagged_when_no_lead_and_workers_present() {
        // Workers (with newer mtime) must be filtered out; the
        // resolver returns the latest UNTAGGED entry.
        let worker = forge_primitives::worker_tag("reviewer");
        let sessions = vec![
            mk_session("old-untagged", None, 50),
            mk_session("new-untagged", None, 100),
            mk_session("worker", Some(&worker), 999),
        ];
        let picked = resolve_lead_session(&sessions).expect("some");
        assert_eq!(picked.session_id, "new-untagged");
    }

    #[test]
    fn resolver_returns_none_for_empty() {
        assert!(resolve_lead_session(&[]).is_none());
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build a usage snapshot with 5h + shared-7d windows sharing one
    /// `resets_at`. Enough for the `/account` picker snapshot tests.
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
        }
    }

    /// `project_accounts_snapshot` returns one row per allow-list entry
    /// in order, each carrying the account's config_dir, is_current
    /// marker, usable flag, 5h/7d utilization, and a reset ETA only
    /// while the account is at its cap.
    #[test]
    fn project_accounts_snapshot_reports_allowlist_order_and_state() {
        let (ws, _rx) = Workspace::testing_stub();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        {
            let mut map = AccountStateMap::new(&[
                crate::config::LoadedAccount {
                    display_name: "A".to_owned(),
                    config_dir: PathBuf::from("/cfg/A"),
                    proxy: true,
                    env: std::collections::HashMap::new(),
                    experimental: false,
                },
                crate::config::LoadedAccount {
                    display_name: "B".to_owned(),
                    config_dir: PathBuf::from("/cfg/B"),
                    proxy: true,
                    env: std::collections::HashMap::new(),
                    experimental: false,
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
            *ws.accounts.lock() = map;
        }

        let rows = ws.project_accounts_snapshot(&["A".to_owned(), "B".to_owned()], Some("A"));

        assert_eq!(rows.len(), 2, "one row per allow-list entry");
        assert_eq!(rows[0].display_name, "A", "allow-list order preserved");
        assert_eq!(rows[1].display_name, "B");

        // A: current + saturated -> not usable, carries a reset ETA.
        assert!(rows[0].is_current, "A is the session's active account");
        assert!(!rows[0].usable, "A saturated on 5h -> rate limited");
        assert!((rows[0].five_hour_util - 100.0).abs() < f64::EPSILON);
        assert!((rows[0].seven_day_util - 63.0).abs() < f64::EPSILON);
        assert_eq!(rows[0].resets_at, Some(future), "capped account shows when it unlocks");
        assert_eq!(rows[0].config_dir, PathBuf::from("/cfg/A"));

        // B: not current + usable -> no reset ETA.
        assert!(!rows[1].is_current);
        assert!(rows[1].usable, "B under cap on both windows");
        assert!((rows[1].five_hour_util - 34.0).abs() < f64::EPSILON);
        assert!(rows[1].resets_at.is_none(), "usable account has no reset ETA");
    }

    /// Experimental accounts are globally selectable: they appear in the
    /// picker snapshot even when the project's org allow-list does NOT
    /// pin them, flagged experimental and sorted after the regular rows.
    #[test]
    fn project_accounts_snapshot_includes_experimental_globally() {
        let (ws, _rx) = Workspace::testing_stub();
        {
            let mut map = AccountStateMap::new(&[
                crate::config::LoadedAccount {
                    display_name: "A".to_owned(),
                    config_dir: PathBuf::from("/cfg/A"),
                    proxy: true,
                    env: std::collections::HashMap::new(),
                    experimental: false,
                },
                crate::config::LoadedAccount {
                    display_name: "Exp".to_owned(),
                    config_dir: PathBuf::from("/cfg/Exp"),
                    proxy: true,
                    env: std::collections::HashMap::new(),
                    experimental: true,
                },
            ]);
            map.set_usage(&AccountKey("A".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            map.set_usage(&AccountKey("Exp".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            *ws.accounts.lock() = map;
        }

        // Allow-list pins only "A"; "Exp" is a different org's account.
        let rows = ws.project_accounts_snapshot(&["A".to_owned()], Some("A"));

        assert_eq!(rows.len(), 2, "experimental Exp is unioned in despite not being pinned");
        assert_eq!(rows[0].display_name, "A", "regular allow-list rows come first");
        assert!(!rows[0].experimental, "A is a regular account");
        assert_eq!(rows[1].display_name, "Exp", "experimental rows sorted last");
        assert!(rows[1].experimental, "Exp is flagged experimental");
    }

    /// An experimental account that also happens to sit in the project's
    /// allow-list renders exactly once (deduped), flagged experimental.
    #[test]
    fn project_accounts_snapshot_dedups_experimental_in_allowlist() {
        let (ws, _rx) = Workspace::testing_stub();
        {
            let mut map = AccountStateMap::new(&[
                crate::config::LoadedAccount {
                    display_name: "A".to_owned(),
                    config_dir: PathBuf::from("/cfg/A"),
                    proxy: true,
                    env: std::collections::HashMap::new(),
                    experimental: false,
                },
                crate::config::LoadedAccount {
                    display_name: "Exp".to_owned(),
                    config_dir: PathBuf::from("/cfg/Exp"),
                    proxy: true,
                    env: std::collections::HashMap::new(),
                    experimental: true,
                },
            ]);
            map.set_usage(&AccountKey("A".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            map.set_usage(&AccountKey("Exp".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            *ws.accounts.lock() = map;
        }

        // "Exp" is BOTH pinned by the allow-list AND experimental.
        let rows = ws.project_accounts_snapshot(&["A".to_owned(), "Exp".to_owned()], None);

        assert_eq!(rows.len(), 2, "no duplicate row for the already-pinned experimental account");
        let exp_rows: Vec<&crate::AccountRow> =
            rows.iter().filter(|r| r.display_name == "Exp").collect();
        assert_eq!(exp_rows.len(), 1, "Exp appears exactly once");
        assert!(exp_rows[0].experimental, "the deduped Exp row stays flagged experimental");
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
                    config_dir: PathBuf::from("/c/One"),
                    proxy: true,
                    env: std::collections::HashMap::new(),
                    experimental: false,
                },
                crate::config::LoadedAccount {
                    display_name: "Two".to_owned(),
                    config_dir: PathBuf::from("/c/Two"),
                    proxy: true,
                    env: std::collections::HashMap::new(),
                    experimental: false,
                },
            ]);
            map.set_usage(&AccountKey("One".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            map.set_usage(&AccountKey("Two".to_owned()), account_usage_snapshot(10.0, 10.0, None));
            *ws.accounts.lock() = map;
        }

        let rows = ws.project_accounts_snapshot(&[], None);
        let names: Vec<&str> = rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(names, vec!["One", "Two"], "empty pin lists all accounts in order");
        assert!(rows.iter().all(|r| r.usable), "both under cap -> usable");
        assert!(rows.iter().all(|r| !r.is_current), "no current account when None passed");
    }

    /// The supersession guard that keeps an `/account` switch's
    /// re-spawn intact: a stale predecessor task exiting must NOT wipe
    /// the successor's pool entry, command sender, or domain handle -
    /// all three are gated together on `Arc` identity. The current
    /// owner's own exit still releases all three.
    #[test]
    fn release_session_if_current_is_supersession_safe() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = SessionKey::from_str_for_test("sup-key");

        let (ha, _rxa) = Workspace::testing_stub_handle();
        let arc_a = Arc::new(ha);
        let (hb, _rxb) = Workspace::testing_stub_handle();
        let arc_b = Arc::new(hb);

        // The successor (account B) owns all three registrations.
        let (tx, _cmd_rx) = mpsc::unbounded_channel::<Command>();
        ws.pool.lock().insert(
            key.clone(),
            PooledAgent { handle: Arc::clone(&arc_b), account: AccountKey("B".to_owned()) },
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
    fn force_new_gate_overrides_present_lead() {
        let lead = SessionKey::from_session_id("lead-uuid");
        // Normal boot (force_new = false): a resumable catalog lead is
        // resumed.
        assert_eq!(Workspace::apply_force_new_gate(Some(lead.clone()), false), Some(lead.clone()),);
        // `--new` (force_new = true): the present lead is skipped, so
        // the spawn falls to new_session - this is what makes
        // `forge <project> --new` (which boots the focused lead via
        // StartDefault -> the same gate) come up fresh.
        assert_eq!(Workspace::apply_force_new_gate(Some(lead), true), None);
        // No catalog lead: fresh either way.
        assert_eq!(Workspace::apply_force_new_gate(None, false), None);
        assert_eq!(Workspace::apply_force_new_gate(None, true), None);
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

    #[test]
    fn review_thread_crud_round_trips_through_the_workspace() {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        let make = |id: &str, line: u32| ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line,
                content_hash: u64::from(line),
                context: vec!["ctx".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: format!("c{id}"),
                at: "2026-07-19T10:00:00Z".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-19T10:00:00Z".to_owned(),
            updated_at: "2026-07-19T10:00:00Z".to_owned(),
            commit: None,
        };

        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        assert!(ws.load_review_threads("forge", "feat").expect("load").is_empty(), "empty on miss");

        ws.upsert_review_thread("forge", "feat", make("a", 10));
        ws.upsert_review_thread("forge", "feat", make("b", 20));
        assert_eq!(ws.load_review_threads("forge", "feat").expect("load").len(), 2);

        ws.set_review_thread_status("forge", "feat", "a", ReviewStatus::Resolved);
        let loaded = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(loaded.iter().find(|t| t.id == "a").expect("a").status, ReviewStatus::Resolved);
        assert_eq!(loaded.iter().find(|t| t.id == "b").expect("b").status, ReviewStatus::Open);

        // A different (project, branch) is scoped separately.
        ws.save_review_threads("forge", "other", &[make("c", 30)]);
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load").len(),
            2,
            "other branch is isolated"
        );
        assert_eq!(ws.load_review_threads("forge", "other").expect("load").len(), 1);

        ws.delete_review_threads("forge", "feat");
        assert!(
            ws.load_review_threads("forge", "feat").expect("load").is_empty(),
            "teardown clears the branch"
        );
        assert_eq!(
            ws.load_review_threads("forge", "other").expect("load").len(),
            1,
            "delete is scoped"
        );
    }

    #[test]
    fn submit_review_files_listed_threads_through_the_workspace() {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        let make = |id: &str| ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 1,
                content_hash: 1,
                context: vec!["ctx".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: format!("c{id}"),
                at: "2026-07-23T10:00:00Z".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-23T10:00:00Z".to_owned(),
            updated_at: "2026-07-23T10:00:00Z".to_owned(),
            commit: None,
        };

        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        assert!(ws.load_reviews("forge", "feat").expect("load").is_empty(), "empty on miss");
        ws.save_review_threads("forge", "feat", &[make("a"), make("b"), make("c")]);

        let filed = |id: &str| {
            ws.load_review_threads("forge", "feat")
                .expect("load")
                .into_iter()
                .find(|t| t.id == id)
                .expect("thread")
                .origin_review()
                .map(str::to_owned)
        };

        let origin = SessionKey::from_session_id("reviewer");
        let r1 = ws
            .submit_review(
                "forge",
                "feat",
                Some("first".to_owned()),
                &["a".to_owned(), "b".to_owned()],
                origin.clone(),
            )
            .expect("submit mints a review");
        assert_eq!(r1.number, 1);
        assert_eq!(filed("a"), Some(r1.id.clone()), "a filed");
        assert_eq!(filed("b"), Some(r1.id.clone()), "b filed");
        assert_eq!(filed("c"), None, "c unlisted stays unfiled");
        assert_eq!(ws.load_reviews("forge", "feat").expect("load").len(), 1);

        let r2 =
            ws.submit_review("forge", "feat", None, &["c".to_owned()], origin).expect("submit 2");
        assert_eq!(r2.number, 2, "the number increments");
        assert_eq!(filed("a"), Some(r1.id.clone()), "a stays in r1");
        assert_eq!(filed("c"), Some(r2.id), "c filed into r2");
    }

    #[test]
    fn review_conversation_methods_round_trip_through_the_workspace() {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        let make = |id: &str| ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 12,
                content_hash: 1,
                context: vec!["fn f() {".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: format!("look at {id}"),
                at: "2026-07-23T10:00:00Z".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-23T10:00:00Z".to_owned(),
            updated_at: "2026-07-23T10:00:00Z".to_owned(),
            commit: None,
        };

        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.save_review_threads("forge", "feat", &[make("a"), make("b")]);
        let caller = SessionKey::from_session_id("worker");
        let r1 = ws
            .submit_review(
                "forge",
                "feat",
                Some("overview".to_owned()),
                &["a".to_owned(), "b".to_owned()],
                SessionKey::from_session_id("reviewer"),
            )
            .expect("submit");

        let list = ws.review_list("forge", "feat").expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].review_id, r1.id);
        assert_eq!(list[0].comment_count, 2);
        assert_eq!(list[0].open, 2);

        let detail = ws.review_get("forge", "feat", &r1.id).expect("get").expect("review present");
        assert_eq!(detail.comments.len(), 2);
        assert_eq!(detail.summary.as_deref(), Some("overview"));
        assert!(!detail.comments[0].context.is_empty(), "anchor context is handed back");

        let status = ws
            .review_reply(
                &caller,
                "forge",
                "feat",
                "a",
                "implementer",
                "fixed",
                "2026-07-23T12:00:00Z",
            )
            .expect("reply");
        assert_eq!(status, ReviewStatus::Addressed, "a reply flips Open -> Addressed");
        let list = ws.review_list("forge", "feat").expect("list");
        assert_eq!((list[0].open, list[0].addressed), (1, 1));

        ws.review_resolve(&caller, "forge", "feat", "a").expect("resolve");
        let list = ws.review_list("forge", "feat").expect("list");
        assert_eq!((list[0].addressed, list[0].resolved), (0, 1));

        // An unknown / cross-branch comment id is rejected, not a no-op -
        // for reply (append_reply path) and resolve (set_status path) alike.
        assert!(ws.review_reply(&caller, "forge", "feat", "missing", "x", "y", "z").is_err());
        assert!(ws.review_resolve(&caller, "forge", "feat", "missing").is_err());
        assert!(
            ws.review_reply(&caller, "forge", "other", "a", "x", "y", "z").is_err(),
            "reply: a lives on feat, not other",
        );
        assert!(
            ws.review_resolve(&caller, "forge", "other", "a").is_err(),
            "resolve: a lives on feat, not other",
        );
    }

    #[test]
    fn drain_review_activity_batches_one_notice_per_review_routed_to_origin() {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        let make = |id: &str| ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 1,
                content_hash: 1,
                context: vec!["ctx".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: format!("look at {id}"),
                at: "2026-07-23T10:00:00Z".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-23T10:00:00Z".to_owned(),
            updated_at: "2026-07-23T10:00:00Z".to_owned(),
            commit: None,
        };
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.save_review_threads("forge", "feat", &[make("a"), make("b"), make("c")]);
        let reviewer = SessionKey::from_session_id("reviewer");
        let worker = SessionKey::from_session_id("worker");
        ws.submit_review(
            "forge",
            "feat",
            None,
            &["a".to_owned(), "b".to_owned(), "c".to_owned()],
            reviewer.clone(),
        )
        .expect("submit");

        // Nothing before the worker acts.
        assert!(ws.drain_review_activity(&worker).is_empty(), "no activity, no notice");

        // Worker replies to one comment and resolves another in one turn,
        // leaving the third (c) untouched so an open count survives.
        ws.review_reply(&worker, "forge", "feat", "a", "impl", "fixed a", "t").expect("reply a");
        ws.review_resolve(&worker, "forge", "feat", "b").expect("resolve b");

        let notices = ws.drain_review_activity(&worker);
        assert_eq!(notices.len(), 1, "one batched notice for the single touched review");
        match &notices[0] {
            SessionUpdate::ReviewActivityNotice { key, branch, waiting, message } => {
                assert_eq!(key, &reviewer, "routed to the submit origin, not the worker");
                assert_eq!(branch, "feat", "the notice names the branch it is about");
                // a -> Addressed (1 replied), b -> Resolved (1 resolved), c untouched (1 open).
                assert_eq!(*waiting, 1, "only the replied-to thread awaits the reviewer");
                assert!(message.contains("1 replied"), "tally counts the reply: {message}");
                assert!(message.contains("1 resolved"), "tally counts the resolve: {message}");
                assert!(
                    message.contains("1 open"),
                    "tally reflects the store's open count: {message}"
                );
            }
            other => panic!("expected ReviewActivityNotice, got {other:?}"),
        }
        // The buffer is drained - a second flush is empty.
        assert!(ws.drain_review_activity(&worker).is_empty(), "the turn buffer drained");
    }

    #[test]
    fn drain_drops_notice_when_no_submit_origin_recorded() {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSet, ReviewSide, ReviewStatus,
            ReviewThread,
        };
        // Post-restart shape: a thread already filed into a review on disk
        // (review_id set) + its reviews row, but `review_origin` is empty
        // (in-memory, cleared by the restart) because we seed the store
        // directly instead of calling submit_review. A worker reply must
        // then drain to NO notice - never mis-routed to the worker.
        let dir = tempdir().expect("tempdir");
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        let thread = ReviewThread {
            id: "a".to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 1,
                content_hash: 1,
                context: vec!["ctx".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: "look".to_owned(),
                at: "2026-07-23T10:00:00Z".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-23T10:00:00Z".to_owned(),
            updated_at: "2026-07-23T10:00:00Z".to_owned(),
            commit: None,
        };
        crate::store::review::save(&db, "forge", "feat", &[thread]).expect("seed threads");
        crate::store::review::save_reviews(
            &db,
            "forge",
            "feat",
            &[ReviewSet {
                id: "r1".to_owned(),
                number: 1,
                summary: None,
                created_at: "2026-07-23T10:00:00Z".to_owned(),
            }],
        )
        .expect("seed reviews");

        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(db);
        let worker = SessionKey::from_session_id("worker");
        ws.review_reply(&worker, "forge", "feat", "a", "impl", "fixed", "t").expect("reply");

        assert!(
            ws.drain_review_activity(&worker).is_empty(),
            "with no recorded origin the notice is dropped, not mis-routed to the worker",
        );
    }

    #[test]
    fn submit_and_drain_do_not_deadlock_under_concurrency() {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        let make = |id: &str| ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 1,
                content_hash: 1,
                context: vec!["ctx".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: format!("look at {id}"),
                at: "2026-07-23T10:00:00Z".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-23T10:00:00Z".to_owned(),
            updated_at: "2026-07-23T10:00:00Z".to_owned(),
            commit: None,
        };
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.save_review_threads("forge", "feat", &[make("a"), make("b"), make("c")]);
        let reviewer = SessionKey::from_session_id("reviewer");
        let worker = SessionKey::from_session_id("worker");
        ws.submit_review(
            "forge",
            "feat",
            None,
            &["a".to_owned(), "b".to_owned(), "c".to_owned()],
            reviewer.clone(),
        );

        // Hammer the two lock-ordering-sensitive paths from separate threads:
        // the TUI submit path (`db` then `review_origin`) and the worker
        // turn-end path (`review_origin` after `db`). A regression that
        // reintroduces the opposite order deadlocks; a watchdog channel
        // fails the test on timeout rather than hanging forever.
        let (tx, rx) = std::sync::mpsc::channel();
        let ws_submit = ws.clone();
        let reviewer2 = reviewer.clone();
        let ws_drain = ws.clone();
        let worker2 = worker.clone();
        let coordinator = std::thread::spawn(move || {
            let submitter = std::thread::spawn(move || {
                for _ in 0..300 {
                    ws_submit.submit_review("forge", "feat", None, &[], reviewer2.clone());
                }
            });
            let drainer = std::thread::spawn(move || {
                for _ in 0..300 {
                    let _ = ws_drain.review_reply(&worker2, "forge", "feat", "a", "impl", "x", "t");
                    let _ = ws_drain.drain_review_activity(&worker2);
                }
            });
            let _ = submitter.join();
            let _ = drainer.join();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(20)).is_ok(),
            "submit_review + drain_review_activity deadlocked (AB-BA on db vs review_origin)",
        );
        let _ = coordinator.join();
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
    fn cron_methods_push_list_remove_and_persist() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        let persisted = || {
            crate::store::cron::list(ws.db.lock().as_ref().expect("db installed")).expect("list")
        };

        let entry = CronEntry {
            id: CronId::from("c1"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };

        ws.push_cron(entry.clone());
        assert_eq!(ws.crons_for_project("forge"), vec![entry.clone()], "listed for its project");
        assert!(ws.crons_for_project("other").is_empty(), "scoped by project name");
        assert_eq!(persisted(), vec![entry.clone()], "push persisted to the store");

        // A different project cannot delete another project's cron.
        assert!(!ws.remove_cron("other", &entry.id), "delete is scoped to the owning project");
        assert_eq!(ws.crons_for_project("forge").len(), 1);

        assert!(ws.remove_cron("forge", &entry.id), "the owning project removes it");
        assert!(ws.crons_for_project("forge").is_empty());
        assert!(persisted().is_empty(), "removal persisted");

        assert!(!ws.remove_cron("forge", &CronId::from("c1")), "removing a gone id reports false");
    }

    #[test]
    fn project_name_for_path_resolves_by_cwd_and_degrades_cleanly() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());

        let path = "/tmp/cron-project-path-proj";
        ws.seed_test_project_with_static_workers("cronproj", path, &[]);

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
        ws.seed_test_project_with_static_workers("symproj", &root.to_string_lossy(), &[]);

        let absent_worktree = root.join(".claude").join("worktrees").join("reviewer");
        assert_eq!(
            ws.project_name_for_path(&absent_worktree.to_string_lossy()).as_deref(),
            Some("symproj"),
            "an absent worktree subdir under a symlinked-ancestor root resolves to its parent",
        );
    }

    // forge.toml for the account-pin resolution tests: a `subspace`
    // project pinned to ["Subspace"] plus an alphabetically-earlier
    // auto_start default ("busymail") on the Granite org, so a miss
    // surfaces as the Granite pin rather than an empty list / panic.
    const ACCOUNT_PIN_FIXTURE: &str = r#"
[[orgs]]
name = "Granite"
accounts = ["Granite", "Granite1", "Personal"]
[[orgs.projects]]
name = "busymail"
path = "/tmp/wt-acct-busymail"
auto_start = true

[[orgs]]
name = "Subspace"
accounts = ["Subspace"]
[[orgs.projects]]
name = "subspace"
path = "/tmp/wt-acct-subspace"
auto_start = true

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/wt-acct-cfg/granite"
[[accounts]]
display_name = "Granite1"
config_dir = "/tmp/wt-acct-cfg/granite1"
[[accounts]]
display_name = "Personal"
config_dir = "/tmp/wt-acct-cfg/personal"
[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/wt-acct-cfg/subspace"
"#;

    // Stub whose config is `ACCOUNT_PIN_FIXTURE`. The returned `TempDir`
    // guards the config dir for the caller's lifetime.
    fn stub_with_account_pin_fixture() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
        fs::write(forge_dir.join("forge.toml"), ACCOUNT_PIN_FIXTURE).expect("write forge.toml");
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_owned(), config);
        (ws, dir)
    }

    /// Same shape as `ACCOUNT_PIN_FIXTURE` but with per-project env, so
    /// a spawn-target arm that stops resolving shows up as the project's
    /// keys going missing rather than as a silent pass.
    const PROJECT_ENV_FIXTURE: &str = r#"
[[orgs]]
name = "Granite"
accounts = ["Granite"]
[[orgs.projects]]
name = "busymail"
path = "/tmp/wt-env-busymail"
auto_start = true

[[orgs]]
name = "Subspace"
accounts = ["Subspace"]
[[orgs.projects]]
name = "subspace"
path = "/tmp/wt-env-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/wt-env-cfg/granite"
[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/wt-env-cfg/subspace"

[projects.busymail.env]
BUSYMAIL_TOKEN = "busymail-value"
[projects.subspace.env]
SUBSPACE_TOKEN = "subspace-value"
"#;

    fn stub_with_project_env_fixture() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
        fs::write(forge_dir.join("forge.toml"), PROJECT_ENV_FIXTURE).expect("write forge.toml");
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_owned(), config);
        (ws, dir)
    }

    /// `SessionTarget::Default` is the alphabetically-first auto_start
    /// project, and its env must reach the spawn.
    #[test]
    fn session_env_for_default_target_applies_the_default_projects_env() {
        let (ws, _dir) = stub_with_project_env_fixture();
        let env = ws.session_env_for(&SessionTarget::Default, &std::collections::HashMap::new());
        assert_eq!(
            env.get("BUSYMAIL_TOKEN").map(String::as_str),
            Some("busymail-value"),
            "the Default arm must resolve to the default project, not None",
        );
        assert!(!env.contains_key("SUBSPACE_TOKEN"), "and not to any other project");
    }

    /// `FreshInProject` is the workers-MCP spawn path. If this arm stops
    /// resolving, a worker runs without its project's declared keys and
    /// the symptom surfaces inside the worker rather than at boot.
    #[test]
    fn session_env_for_fresh_in_project_applies_that_projects_env() {
        let (ws, _dir) = stub_with_project_env_fixture();
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                "/tmp/wt-env-subspace",
            )));
        let target = SessionTarget::FreshInProject {
            project_key,
            synth_key: SessionKey::from_session_id("__spawn_subspace__"),
        };
        let env = ws.session_env_for(&target, &std::collections::HashMap::new());
        assert_eq!(
            env.get("SUBSPACE_TOKEN").map(String::as_str),
            Some("subspace-value"),
            "a fresh worker spawn must get its own project's env",
        );
        assert!(!env.contains_key("BUSYMAIL_TOKEN"), "and not the default project's");
    }

    #[test]
    fn project_accounts_for_resolves_worktree_session_to_parent_project() {
        // A session whose cwd is a worktree under the subspace root must
        // inherit ["Subspace"], not the alpha-first Granite default.
        let (ws, _dir) = stub_with_account_pin_fixture();
        ws.record_connected_session(
            "/tmp/wt-acct-subspace/.claude/worktrees/reviewer",
            "sess-worktree",
            None,
        );
        let target = SessionTarget::Session(SessionKey::from_session_id("sess-worktree"));
        assert_eq!(
            ws.project_accounts_for(&target),
            vec!["Subspace".to_owned()],
            "a worktree session inherits its parent project's account pin, not the default's",
        );
    }

    #[test]
    fn project_accounts_for_resolves_project_root_session_to_its_own_pin() {
        // The common non-worktree path the fix also rewrote: a session
        // rooted exactly at a project resolves to that project's own pin,
        // not the default.
        let (ws, _dir) = stub_with_account_pin_fixture();
        ws.record_connected_session("/tmp/wt-acct-subspace", "sess-root", None);
        let target = SessionTarget::Session(SessionKey::from_session_id("sess-root"));
        assert_eq!(
            ws.project_accounts_for(&target),
            vec!["Subspace".to_owned()],
            "a project-root session resolves to its own pin",
        );
    }

    #[test]
    fn project_accounts_for_falls_back_to_default_for_unknown_cwd() {
        // A cwd under no configured project degrades to the default
        // project's pin (the alpha-first Granite auto_start default) so
        // the picker always has a non-empty allow-list.
        let (ws, _dir) = stub_with_account_pin_fixture();
        ws.record_connected_session("/tmp/unrelated-elsewhere", "sess-unknown", None);
        let target = SessionTarget::Session(SessionKey::from_session_id("sess-unknown"));
        assert_eq!(
            ws.project_accounts_for(&target),
            vec!["Granite".to_owned(), "Granite1".to_owned(), "Personal".to_owned()],
            "an unknown cwd falls back to the default project's accounts",
        );
    }

    #[test]
    fn durable_gotify_subscription_persists_ephemeral_stays_in_memory() {
        use forge_primitives::GotifySubscription;

        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);

        let sub = |team_role: Option<&str>| GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: "p".to_owned(),
            team_role: team_role.map(str::to_owned),
            applications: vec![],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let durable = sub(None);
        let ephemeral = sub(Some("scratch"));
        ws.add_gotify_subscription(durable.clone(), true);
        ws.add_gotify_subscription(ephemeral, false);

        assert_eq!(
            ws.gotify_subscriptions_for_project("p").len(),
            2,
            "both live in the active in-memory set",
        );
        let persisted = || {
            crate::store::gotify::list(ws.db.lock().as_ref().expect("db installed")).expect("list")
        };
        assert_eq!(persisted().len(), 1, "only the durable subscription hit redb");
        assert_eq!(persisted()[0].id, durable.id);

        assert!(
            ws.remove_gotify_subscription_owned_by("p", durable.id, None),
            "the lead's own durable id removes",
        );
        assert!(persisted().is_empty(), "removal cleared the persisted record");
    }

    fn dynamic_worker_row(
        project: &str,
        label: &str,
    ) -> crate::store::dynamic_workers::DynamicWorker {
        crate::store::dynamic_workers::DynamicWorker {
            project_key: project.to_owned(),
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            kick: Some(format!("kick for {label}")),
        }
    }

    fn live_worker_entry(label: &str, key: &str) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.to_owned(),
            charter: "c".to_owned(),
            session_key: SessionKey::from_session_id(key),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".to_owned(),
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
        let project = ProjectKey::new("forge");

        let _ = ws.persist_dynamic_worker(&dynamic_worker_row("forge", "reviewer"));
        let _ = ws.persist_dynamic_worker(&dynamic_worker_row("forge", "tester"));
        ws.insert_live_worker(&project, live_worker_entry("reviewer", "worker-1"));
        ws.insert_live_worker(&project, live_worker_entry("tester", "worker-2"));

        crate::spawn::handle_close_worker(&ws, &project, "reviewer");

        let rows = {
            let guard = ws.db.lock();
            crate::store::dynamic_workers::list_for_project(
                guard.as_ref().expect("db installed"),
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
        let project = ProjectKey::new("forge");

        let _ = ws.persist_dynamic_worker(&dynamic_worker_row("forge", "reviewer"));
        ws.insert_live_worker(&project, live_worker_entry("reviewer", "worker-1"));

        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::spawn::handle_despawn_worker(&ws, &project, "reviewer", false, tx);
        assert!(matches!(rx.await, Ok(crate::protocol::DespawnResult::Despawned { .. })));

        let rows = {
            let guard = ws.db.lock();
            crate::store::dynamic_workers::list_for_project(
                guard.as_ref().expect("db installed"),
                "forge",
            )
            .expect("list")
        };
        assert!(rows.is_empty(), "despawn deletes the persisted dynamic-worker row");
    }

    /// `remove_gotify_subscriptions_for_worker` drops only the target
    /// worker's subs (matched by project name + label) from both the
    /// in-memory set and redb; the lead's sub and a sibling worker's sub
    /// survive, proving the removal is scoped, not a blanket project wipe.
    #[test]
    fn remove_gotify_subscriptions_for_worker_is_scoped_to_project_and_label() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project_with_static_workers("forge", "/tmp/gotify-durability", &[]);
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");

        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        let lead_sub = gotify_sub("forge", &[], None);
        let mut sibling_sub = gotify_sub("forge", &[], None);
        sibling_sub.team_role = Some("other".to_owned());
        ws.add_gotify_subscription(scratch_sub.clone(), true);
        ws.add_gotify_subscription(lead_sub.clone(), true);
        ws.add_gotify_subscription(sibling_sub.clone(), true);

        ws.remove_gotify_subscriptions_for_worker(&view_key, "scratch");

        let in_mem = ws.gotify_subscriptions_for_project("forge");
        assert!(
            in_mem.iter().all(|s| s.id != scratch_sub.id),
            "the scratch worker's sub is gone from memory",
        );
        assert!(
            in_mem.iter().any(|s| s.id == lead_sub.id)
                && in_mem.iter().any(|s| s.id == sibling_sub.id),
            "the lead sub and the sibling worker's sub survive in memory",
        );

        let persisted = {
            let guard = ws.db.lock();
            crate::store::gotify::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted.iter().all(|s| s.id != scratch_sub.id),
            "the scratch worker's sub is gone from redb",
        );
        assert!(
            persisted.iter().any(|s| s.id == lead_sub.id)
                && persisted.iter().any(|s| s.id == sibling_sub.id),
            "the survivors are still persisted in redb",
        );
    }

    /// `teardown_worker` (the shared Projects-pane close + workers__despawn
    /// routine) drops the worker's durable Gotify subs alongside its
    /// dynamic-worker row; the lead's sub survives.
    #[tokio::test]
    async fn teardown_worker_drops_the_workers_durable_gotify_subs() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project_with_static_workers("forge", "/tmp/gotify-durability", &[]);
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");
        ws.insert_live_worker(&view_key, live_worker_entry("scratch", "worker-1"));

        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        let lead_sub = gotify_sub("forge", &[], None);
        ws.add_gotify_subscription(scratch_sub.clone(), true);
        ws.add_gotify_subscription(lead_sub.clone(), true);

        crate::spawn::teardown_worker(&ws, &view_key, "scratch");

        let in_mem = ws.gotify_subscriptions_for_project("forge");
        assert!(
            in_mem.iter().all(|s| s.id != scratch_sub.id),
            "teardown removed the worker's sub from memory",
        );
        assert!(in_mem.iter().any(|s| s.id == lead_sub.id), "the lead sub survives teardown");

        let persisted = {
            let guard = ws.db.lock();
            crate::store::gotify::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted.iter().all(|s| s.id != scratch_sub.id),
            "teardown removed the worker's sub from redb",
        );
        assert!(persisted.iter().any(|s| s.id == lead_sub.id), "the lead sub is still persisted");
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
    async fn teardown_dynamic_worker_drops_its_crons_and_subs_keeps_others() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // Empty static_workers -> "scratch" is a dynamic worker.
        ws.seed_test_project_with_static_workers("forge", "/tmp/cron-teardown-dyn", &[]);
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

        crate::spawn::teardown_worker(&ws, &view_key, "scratch");

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

    #[tokio::test]
    async fn teardown_static_worker_keeps_its_crons_and_subs() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // "reviewer" IS a static worker -> it re-spawns, so a close keeps
        // its durable state.
        ws.seed_test_project_with_static_workers(
            "forge",
            "/tmp/cron-teardown-static",
            &["reviewer".to_owned()],
        );
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");
        ws.insert_live_worker(&view_key, live_worker_entry("reviewer", "worker-1"));

        let reviewer_cron = worker_cron("reviewer-cron", "forge", Some("reviewer"));
        ws.push_cron(reviewer_cron.clone());
        let mut reviewer_sub = gotify_sub("forge", &[], None);
        reviewer_sub.team_role = Some("reviewer".to_owned());
        ws.add_gotify_subscription(reviewer_sub.clone(), true);

        crate::spawn::teardown_worker(&ws, &view_key, "reviewer");

        assert!(
            ws.crons_for_project("forge").iter().any(|c| c.id == reviewer_cron.id),
            "a static worker keeps its cron on close",
        );
        assert!(
            ws.gotify_subscriptions_for_project("forge").iter().any(|s| s.id == reviewer_sub.id),
            "a static worker keeps its sub on close",
        );
        let persisted_crons = {
            let guard = ws.db.lock();
            crate::store::cron::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted_crons.iter().any(|c| c.id == reviewer_cron.id),
            "the static worker's cron stays in the store",
        );
    }

    /// The seam the bug lived in: `resolve_identity` gathers the caller's
    /// dynamic-worker labels from the table and marks a table-backed
    /// worker durable, so its sub persists through the subscribe path even
    /// though the worker is neither a static worker nor the lead.
    #[test]
    fn resolve_identity_persists_a_table_backed_dynamic_workers_sub() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project_with_static_workers("forge", "/tmp/gotify-durability-seam", &[]);
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");

        // "scratch" is neither a static worker nor the lead: durability
        // must come solely from its dynamic_workers row.
        let _ = ws.persist_dynamic_worker(&dynamic_worker_row(view_key.as_str(), "scratch"));
        let caller = SessionKey::from_session_id("scratch-session");
        ws.insert_live_worker(&view_key, live_worker_entry("scratch", "scratch-session"));

        let (name, team_role, durable) = crate::mcp::gotify::facade::resolve_identity(&ws, &caller)
            .expect("the worker caller resolves to its project");
        assert_eq!(
            (name.as_str(), team_role.as_deref(), durable),
            ("forge", Some("scratch"), true),
            "a table-backed dynamic worker resolves as a durable subscriber",
        );

        // The durable identity persists through the subscribe path's write.
        let sub = forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: name,
            team_role,
            applications: vec![],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let sub_id = sub.id;
        ws.add_gotify_subscription(sub, durable);
        let persisted = {
            let guard = ws.db.lock();
            crate::store::gotify::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted.iter().any(|s| s.id == sub_id),
            "the table-backed worker's sub is persisted to redb",
        );
    }

    /// A `ProjectKey` that matches no project resolves to no name, so the
    /// removal is a safe no-op that leaves every sub intact.
    #[test]
    fn remove_gotify_subscriptions_for_worker_unknown_project_is_a_no_op() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project_with_static_workers("forge", "/tmp/gotify-durability-noop", &[]);

        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        ws.add_gotify_subscription(scratch_sub.clone(), true);

        ws.remove_gotify_subscriptions_for_worker(&ProjectKey::new("ghost"), "scratch");

        assert!(
            ws.gotify_subscriptions_for_project("forge").iter().any(|s| s.id == scratch_sub.id),
            "an unknown project key leaves the in-memory subs intact",
        );
        let persisted = {
            let guard = ws.db.lock();
            crate::store::gotify::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted.iter().any(|s| s.id == scratch_sub.id),
            "and leaves the persisted subs intact",
        );
    }

    /// With no store installed the removal still scrubs the in-memory set;
    /// the redb delete is simply skipped (mirrors
    /// `persist_dynamic_worker_errors_when_store_unavailable`).
    #[test]
    fn remove_gotify_subscriptions_for_worker_scrubs_memory_without_a_db() {
        let (ws, _rx) = Workspace::testing_stub();
        // No install_db_for_test: the store is closed for this session.
        ws.seed_test_project_with_static_workers("forge", "/tmp/gotify-durability-nodb", &[]);
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");

        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        ws.add_gotify_subscription(scratch_sub.clone(), true);

        ws.remove_gotify_subscriptions_for_worker(&view_key, "scratch");

        assert!(
            ws.gotify_subscriptions_for_project("forge").iter().all(|s| s.id != scratch_sub.id),
            "the in-memory sub is scrubbed even with no store installed",
        );
    }

    /// #3: persisting reports failure (rather than swallowing it) when
    /// the store is unavailable, so the MCP spawn path can warn the lead
    /// that the worker won't survive a restart.
    #[test]
    fn persist_dynamic_worker_errors_when_store_unavailable() {
        let (ws, _rx) = Workspace::testing_stub();
        // No install_db_for_test: the store is closed for this session.
        let result = ws.persist_dynamic_worker(&dynamic_worker_row("forge", "reviewer"));
        assert!(
            result.is_err(),
            "a closed store surfaces a durability failure, not a silent no-op"
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

    fn gotify_msg(appid: u64, priority: u8) -> forge_primitives::GotifyMessage {
        forge_primitives::GotifyMessage {
            id: 1,
            appid,
            title: "Alert".to_owned(),
            message: "body".to_owned(),
            priority,
            date: "2026-07-03T00:00:00Z".to_owned(),
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

    fn gotify_deliveries(cmds: &[crate::protocol::Command]) -> Vec<(String, String)> {
        cmds.iter()
            .filter_map(|c| match c {
                crate::protocol::Command::DeliverGotifyMessage {
                    project, notification, ..
                } => Some((project.clone(), notification.to_prose())),
                _ => None,
            })
            .collect()
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

    #[test]
    fn route_gotify_message_fans_out_to_all_matches() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.add_gotify_subscription(gotify_sub("p1", &[], None), false);
        ws.add_gotify_subscription(gotify_sub("p2", &[], None), false);

        ws.enable_test_dispatch_intercept();
        ws.route_gotify_message(&gotify_msg(3, 5));
        let deliveries = gotify_deliveries(&ws.drain_test_dispatch_buffer());

        assert_eq!(deliveries.len(), 2, "both matching subscriptions deliver");
        let projects: Vec<&str> = deliveries.iter().map(|(p, _)| p.as_str()).collect();
        assert!(projects.contains(&"p1") && projects.contains(&"p2"), "one delivery per project");
        assert!(
            deliveries[0].1.contains("Alert") && deliveries[0].1.contains("body"),
            "the envelope carries the title and message",
        );
    }

    #[test]
    fn route_gotify_message_respects_priority_and_application_filters() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        *ws.gotify_app_index.lock() = HashMap::from([("alerts".to_owned(), 3u64)]);
        ws.add_gotify_subscription(gotify_sub("p", &["alerts"], None), false);
        ws.add_gotify_subscription(gotify_sub("p", &[], Some(5)), false);

        ws.enable_test_dispatch_intercept();

        // appid 3 (alerts), priority 2: the app-filter sub matches; the
        // priority-floor sub does not (2 < 5).
        ws.route_gotify_message(&gotify_msg(3, 2));
        assert_eq!(gotify_deliveries(&ws.drain_test_dispatch_buffer()).len(), 1);

        // appid 9 (unknown app), priority 5: the priority-floor sub matches;
        // the app-filter sub does not (9 != 3).
        ws.route_gotify_message(&gotify_msg(9, 5));
        assert_eq!(gotify_deliveries(&ws.drain_test_dispatch_buffer()).len(), 1);
    }

    #[test]
    fn route_gotify_message_no_match_delivers_nothing() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.add_gotify_subscription(gotify_sub("p", &[], Some(9)), false);

        ws.enable_test_dispatch_intercept();
        ws.route_gotify_message(&gotify_msg(3, 2));
        assert!(
            gotify_deliveries(&ws.drain_test_dispatch_buffer()).is_empty(),
            "a message below the priority floor delivers nothing",
        );
    }

    #[test]
    fn route_gotify_message_matches_any_app_in_the_set() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        *ws.gotify_app_index.lock() =
            HashMap::from([("alerts".to_owned(), 3u64), ("backups".to_owned(), 7u64)]);
        ws.add_gotify_subscription(gotify_sub("p", &["alerts", "backups"], None), false);

        ws.enable_test_dispatch_intercept();

        // A message from either listed app matches the set.
        ws.route_gotify_message(&gotify_msg(3, 1));
        assert_eq!(gotify_deliveries(&ws.drain_test_dispatch_buffer()).len(), 1, "alerts matches");
        ws.route_gotify_message(&gotify_msg(7, 1));
        assert_eq!(gotify_deliveries(&ws.drain_test_dispatch_buffer()).len(), 1, "backups matches");

        // An app outside the set is skipped.
        ws.route_gotify_message(&gotify_msg(9, 1));
        assert!(
            gotify_deliveries(&ws.drain_test_dispatch_buffer()).is_empty(),
            "an app not in the set delivers nothing",
        );
    }

    #[test]
    fn route_gotify_message_unresolved_appid_does_not_match_numeric_filter() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        // The app index is empty, so appid 5 resolves to no name. A filter
        // listing the numeric id as a string must NOT match on the
        // display-only fallback.
        ws.add_gotify_subscription(gotify_sub("p", &["5"], None), false);

        ws.enable_test_dispatch_intercept();
        ws.route_gotify_message(&gotify_msg(5, 1));
        assert!(
            gotify_deliveries(&ws.drain_test_dispatch_buffer()).is_empty(),
            "an unresolved appid must not match a filter that lists its numeric id",
        );
    }

    #[test]
    fn deliver_gotify_message_spawns_asleep_project_and_buffers() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_static_workers("forge", "/tmp/gotify-forge", &[]);

        let notif = gotify_notif("forge", "Heads up", "hello envelope", 5);
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(&ws, "forge", None, notif.clone());
        let dispatched = ws.drain_test_dispatch_buffer();

        let spawns = dispatched
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
            })
            .count();
        assert_eq!(spawns, 1, "the asleep project got a spawn");

        let synth = SessionKey::from_session_id("__spawn_forge__");
        let buffered = ws
            .domain_handles
            .lock()
            .get(&synth)
            .expect("synth domain present")
            .lock()
            .pending_gotify_prompts
            .clone();
        assert_eq!(buffered, vec![notif], "the notification was buffered");
    }

    #[test]
    fn deliver_gotify_message_skips_gone_project() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        // No project seeded: "ghost" is not in forge.toml.
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(
            &ws,
            "ghost",
            None,
            gotify_notif("ghost", "t", "x", 1),
        );
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, crate::protocol::Command::SpawnProject { .. })),
            "a gone target is skipped without a spawn or panic",
        );
    }

    #[test]
    fn deliver_gotify_message_delivers_to_running_team_worker() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_static_workers(
            "forge",
            "/tmp/gotify-team",
            &["reviewer".to_owned()],
        );
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");
        let worker_key = SessionKey::from_session_id("worker-reviewer");
        ws.insert_live_worker(
            &view_key,
            crate::mcp::workers::types::WorkerEntry {
                label: "reviewer".to_owned(),
                charter: "review".to_owned(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".to_owned(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        ws.mark_session_connected_for_test(&worker_key, "worker-reviewer");
        let notif = gotify_notif("Backups", "Nightly backup", "done", 5);
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(&ws, "forge", Some("reviewer"), notif.clone());
        let dispatched = ws.drain_test_dispatch_buffer();

        assert!(
            dispatched.iter().any(
                |c| matches!(c, crate::protocol::Command::Prompt { key, .. } if key == &worker_key)
            ),
            "a running team worker receives the notification directly",
        );
        assert!(
            dispatched.iter().all(|c| !matches!(c, crate::protocol::Command::SpawnProject { .. })),
            "no project spawn when the target worker is already running",
        );

        // The delivery ALSO echoes the notification block into the worker's
        // chat so the user sees what arrived (mirrors the peer echo).
        let echoed = drain_updates(&mut rx).into_iter().any(|u| matches!(
            u,
            crate::protocol::SessionUpdate::GotifyNotificationAppended { session_id, notification }
                if session_id == worker_key.as_str() && notification == notif
        ));
        assert!(echoed, "a running-target delivery emits a GotifyNotificationAppended echo");
    }

    #[test]
    fn deliver_gotify_message_team_worker_asleep_falls_through_to_lead() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_static_workers(
            "forge",
            "/tmp/gotify-team",
            &["reviewer".to_owned()],
        );

        // No live worker of that role: the subscription falls through to
        // lead delivery, which spawns the asleep project (the lead brings up
        // its team) and buffers the envelope for the Connected drain.
        let notif = gotify_notif("Backups", "backup", "env", 5);
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(&ws, "forge", Some("reviewer"), notif.clone());
        let dispatched = ws.drain_test_dispatch_buffer();

        let spawns = dispatched
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
            })
            .count();
        assert_eq!(spawns, 1, "an asleep team-worker subscription spawns the project lead");

        let synth = SessionKey::from_session_id("__spawn_forge__");
        let buffered = ws
            .domain_handles
            .lock()
            .get(&synth)
            .expect("synth domain present")
            .lock()
            .pending_gotify_prompts
            .clone();
        assert_eq!(buffered, vec![notif], "the notification was buffered");
    }

    /// A team-worker subscription whose worker is a live entry but still
    /// Spawning (session_id None) must buffer on the worker's own
    /// DomainSession for its Connected drain - not dispatch a bare
    /// Command::Prompt (dropped) and not fall through to the lead.
    #[test]
    fn deliver_gotify_message_to_spawning_team_worker_buffers_on_its_domain() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_static_workers(
            "forge",
            "/tmp/gotify-spawning",
            &["reviewer".to_owned()],
        );
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");
        let worker_key = SessionKey::from_session_id("worker-spawning");
        ws.insert_live_worker(&view_key, live_worker_entry("reviewer", "worker-spawning"));
        // Register the domain WITHOUT stamping session_id: still Spawning.
        ws.register_domain_session(worker_key.clone(), None);

        let notif = gotify_notif("Backups", "backup", "env", 5);
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(&ws, "forge", Some("reviewer"), notif.clone());
        let dispatched = ws.drain_test_dispatch_buffer();

        assert!(
            dispatched.is_empty(),
            "no bare Prompt (dropped) and no lead fallback for a spawning worker",
        );
        let buffered = ws
            .domain_session_for(&worker_key)
            .expect("worker domain")
            .lock()
            .pending_gotify_prompts
            .clone();
        assert_eq!(buffered, vec![notif], "buffered on the worker's own domain for its drain");
    }

    #[test]
    fn advance_or_remove_cron_recurring_advances_run_once_removes() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let fired = std::time::SystemTime::now();

        ws.push_cron(CronEntry {
            id: CronId::from("r"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: fired,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        });
        ws.advance_or_remove_cron(&CronId::from("r"), fired);
        let after = ws.crons_for_project("forge");
        assert_eq!(after.len(), 1, "a recurring cron stays after firing");
        assert!(after[0].next_fire > fired, "next_fire advanced to a future slot");
        assert_eq!(after[0].last_fire, Some(fired), "last_fire recorded");

        ws.push_cron(CronEntry {
            id: CronId::from("o"),
            project_name: "forge".to_owned(),
            kind: CronKind::Once(std::time::SystemTime::UNIX_EPOCH),
            prompt: "p".to_owned(),
            created_at: fired,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        });
        ws.advance_or_remove_cron(&CronId::from("o"), fired);
        assert!(
            ws.crons_for_project("forge").iter().all(|c| c.id != CronId::from("o")),
            "a run-once is removed after firing",
        );

        // A missing id is a no-op, not a panic.
        ws.advance_or_remove_cron(&CronId::from("ghost"), fired);
    }

    #[test]
    fn fire_due_crons_spawns_asleep_project_buffers_prompt_and_skips_future() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_static_workers("forge", "/tmp/forge", &[]);

        let now = std::time::SystemTime::now();
        let past = std::time::SystemTime::UNIX_EPOCH;
        let far_future = now + std::time::Duration::from_secs(86_400);

        ws.push_cron(CronEntry {
            id: CronId::from("due"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "morning".to_owned(),
            created_at: past,
            description: None,
            last_fire: None,
            next_fire: past,
            team_role: None,
        });
        ws.push_cron(CronEntry {
            id: CronId::from("later"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "later".to_owned(),
            created_at: past,
            description: None,
            last_fire: None,
            next_fire: far_future,
            team_role: None,
        });

        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);
        let dispatched = ws.drain_test_dispatch_buffer();

        // The asleep project got exactly one SpawnProject - for the due
        // cron, not the future one.
        let spawns = dispatched
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
            })
            .count();
        assert_eq!(spawns, 1, "one spawn for the single due cron");

        // The due cron's prompt is buffered for its owner (the lead) for
        // delivery once the session reaches Connected.
        let buffered: Vec<String> = ws
            .pending_cron_by_owner
            .lock()
            .get(&("forge".to_owned(), None))
            .map(|v| v.iter().map(|p| p.text.clone()).collect())
            .unwrap_or_default();
        assert_eq!(buffered, vec!["morning".to_owned()], "the due cron's prompt was buffered");

        // The due cron advanced past now; the future cron is untouched.
        let crons = ws.crons_for_project("forge");
        let due = crons.iter().find(|c| c.id == CronId::from("due")).expect("due present");
        assert!(due.next_fire > now, "the fired cron advanced past now");
        let later = crons.iter().find(|c| c.id == CronId::from("later")).expect("later present");
        assert_eq!(later.next_fire, far_future, "the not-yet-due cron is untouched");
    }

    #[test]
    fn boot_catch_up_fires_overdue_once_advances_persists_and_does_not_refire() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws.seed_test_project_with_static_workers("forge", "/tmp/forge-catchup", &[]);

        let now = std::time::SystemTime::now();
        let past = std::time::SystemTime::UNIX_EPOCH;
        let far_future = now + std::time::Duration::from_secs(86_400);

        // An overdue recurring + overdue run-once + a not-yet-due one, all
        // for the seeded "forge" project. Boot catch-up is the single
        // fire_due_crons call main.rs makes at startup.
        for entry in [
            CronEntry {
                id: CronId::from("rec"),
                project_name: "forge".to_owned(),
                kind: CronKind::Recurring("*/5 * * * *".to_owned()),
                prompt: "morning".to_owned(),
                created_at: past,
                description: None,
                last_fire: None,
                next_fire: past,
                team_role: None,
            },
            CronEntry {
                id: CronId::from("once"),
                project_name: "forge".to_owned(),
                kind: CronKind::Once(past),
                prompt: "deploy".to_owned(),
                created_at: past,
                description: None,
                last_fire: None,
                next_fire: past,
                team_role: None,
            },
            CronEntry {
                id: CronId::from("future"),
                project_name: "forge".to_owned(),
                kind: CronKind::Recurring("*/5 * * * *".to_owned()),
                prompt: "later".to_owned(),
                created_at: past,
                description: None,
                last_fire: None,
                next_fire: far_future,
                team_role: None,
            },
        ] {
            ws.push_cron(entry);
        }
        assert_eq!(ws.crons_for_project("forge").len(), 3, "all three crons loaded");

        // Boot catch-up: the one call main.rs makes at startup.
        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);
        let first = ws.drain_test_dispatch_buffer();
        let spawns = first
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
            })
            .count();
        assert_eq!(spawns, 2, "boot fires the two overdue crons, not the future one");

        // Overdue recurring advanced past now + recorded last_fire; overdue
        // run-once removed; future untouched.
        let crons = ws.crons_for_project("forge");
        assert!(crons.iter().all(|c| c.id != CronId::from("once")), "overdue run-once removed");
        let rec = crons.iter().find(|c| c.id == CronId::from("rec")).expect("recurring present");
        assert!(rec.next_fire > now, "overdue recurring advanced past now");
        assert_eq!(rec.last_fire, Some(now), "the fired recurring recorded last_fire");
        let fut = crons.iter().find(|c| c.id == CronId::from("future")).expect("future present");
        assert_eq!(fut.next_fire, far_future, "the future cron is untouched");

        // The advance persisted to the store - this is what stops a double-fire.
        let persisted =
            crate::store::cron::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
        assert!(persisted.iter().all(|c| c.id != CronId::from("once")), "removal persisted");
        assert!(
            persisted
                .iter()
                .find(|c| c.id == CronId::from("rec"))
                .expect("rec persisted")
                .next_fire
                > now,
            "advance persisted",
        );

        // The next tick does NOT re-fire: the advanced crons aren't due again.
        ws.fire_due_crons(now);
        let refires = ws
            .drain_test_dispatch_buffer()
            .iter()
            .filter(|c| matches!(c, crate::protocol::Command::SpawnProject { .. }))
            .count();
        assert_eq!(refires, 0, "advanced crons are not due again; no double-fire");
    }

    /// A cron fired into a running lead delivers the raw prompt as a plain
    /// user turn AND echoes a `CronPromptAppended` so the chat shows a cron
    /// block (mirrors the gotify running-target echo). Reproduce-first: the
    /// echo is absent until `deliver_cron_prompt` calls
    /// `push_cron_prompt_into_chat`.
    #[test]
    fn deliver_cron_prompt_to_running_lead_emits_cron_prompt_appended() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_static_workers("cronlead", "/tmp/cron-lead", &[]);
        // Seed the catalog + pool so the project has a running (open) lead:
        // list_projects derives `is_open` from pool membership.
        let cwd = project_expanded_path(&ws, "cronlead");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionKey::from_session_id("lead-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                #[cfg(test)]
                account: AccountKey("test".to_owned()),
            },
        );

        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
        ws.enable_test_dispatch_intercept();
        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, "cronlead", None, "morning".to_owned(), false);
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));

        // The running lead receives the raw cron prompt as a plain user turn.
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::Prompt { key, text, .. } if key == &lead_key && text == "morning"
            )),
            "the running lead receives the fired cron prompt verbatim",
        );

        // AND the delivery echoes a CronPromptAppended so the chat shows a block.
        let echoed = drain_updates(&mut rx).into_iter().any(|u| {
            matches!(
                u,
                SessionUpdate::CronPromptAppended { session_id, text }
                    if session_id == lead_key.as_str() && text == "morning"
            )
        });
        assert!(echoed, "a running-lead cron fire emits a CronPromptAppended echo");
    }

    #[test]
    fn deliver_worker_cron_to_a_live_worker_prompts_the_worker() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project_with_static_workers("proj", "/tmp/wc-live", &["reviewer".to_owned()]);
        let key = ws.list_projects().into_iter().find(|v| v.name == "proj").expect("view").key;
        ws.insert_live_worker(&key, live_worker_entry("reviewer", "worker-uuid"));
        let worker_key = SessionKey::from_session_id("worker-uuid");
        ws.mark_session_connected_for_test(&worker_key, "worker-uuid");

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("reviewer"),
            "review the diff".to_owned(),
            false,
        );
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::Prompt { key, text, .. }
                    if key == &worker_key && text == "review the diff"
            )),
            "a live worker's cron fires straight into the worker",
        );
    }

    #[test]
    fn deliver_asleep_static_worker_cron_buffers_and_wakes_the_project() {
        let (ws, _rx) = Workspace::testing_stub();
        // "reviewer" is a static worker, so the owner exists even while asleep.
        ws.seed_test_project_with_static_workers(
            "proj",
            "/tmp/wc-static",
            &["reviewer".to_owned()],
        );

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("reviewer"),
            "nightly".to_owned(),
            false,
        );
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::SpawnProject { project_name, .. } if project_name == "proj"
            )),
            "an asleep worker cron wakes the whole project via SpawnProject",
        );
        let buffered: Vec<String> = ws
            .pending_cron_by_owner
            .lock()
            .get(&("proj".to_owned(), Some("reviewer".to_owned())))
            .map(|v| v.iter().map(|p| p.text.clone()).collect())
            .unwrap_or_default();
        assert_eq!(buffered, vec!["nightly".to_owned()], "buffered for the worker owner");
    }

    /// A cron for a worker that's a live entry but still Spawning
    /// (session_id None) must NOT dispatch a bare Command::Prompt
    /// (dropped) - the session_id gate in live_cron_owner routes it to
    /// the owner-keyed buffer, drained on the worker's own Connected.
    #[test]
    fn deliver_cron_to_spawning_worker_buffers_via_owner() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project_with_static_workers(
            "proj",
            "/tmp/wc-spawning",
            &["reviewer".to_owned()],
        );
        let key = ws.list_projects().into_iter().find(|v| v.name == "proj").expect("view").key;
        let worker_key = SessionKey::from_session_id("worker-spawning-cron");
        ws.insert_live_worker(&key, live_worker_entry("reviewer", "worker-spawning-cron"));
        // Registered but not connected: session_id stays None.
        ws.register_domain_session(worker_key.clone(), None);

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("reviewer"),
            "nightly".to_owned(),
            false,
        );
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            !dispatched.iter().any(|c| matches!(
                c, Command::Prompt { key, .. } if key == &worker_key
            )),
            "no bare Prompt to the still-spawning worker (would be dropped)",
        );
        let buffered: Vec<String> = ws
            .pending_cron_by_owner
            .lock()
            .get(&("proj".to_owned(), Some("reviewer".to_owned())))
            .map(|v| v.iter().map(|p| p.text.clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            buffered,
            vec!["nightly".to_owned()],
            "buffered for the worker's Connected drain"
        );
    }

    #[tokio::test]
    async fn deliver_asleep_dynamic_worker_cron_buffers_and_wakes_the_project() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project_with_static_workers("proj", "/tmp/wc-dyn", &[]);
        let key = ws.list_projects().into_iter().find(|v| v.name == "proj").expect("view").key;
        // "scratch" exists only via its dynamic_workers row.
        let _ = ws.persist_dynamic_worker(&dynamic_worker_row(key.as_str(), "scratch"));

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("scratch"),
            "hourly".to_owned(),
            false,
        );
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::SpawnProject { project_name, .. } if project_name == "proj"
            )),
            "an asleep dynamic worker cron wakes the project too",
        );
        let count = ws
            .pending_cron_by_owner
            .lock()
            .get(&("proj".to_owned(), Some("scratch".to_owned())))
            .map_or(0, Vec::len);
        assert_eq!(count, 1, "buffered for the dynamic worker owner");
    }

    #[test]
    fn deliver_worker_cron_with_owner_conclusively_gone_is_target_gone() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // Db open + empty dynamic_workers + no static roster: "ghost" is
        // conclusively absent, so its cron is removed.
        ws.seed_test_project_with_static_workers("proj", "/tmp/wc-gone", &[]);
        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, "proj", Some("ghost"), "x".to_owned(), false);
        assert!(
            matches!(outcome, crate::spawn::CronFireOutcome::TargetGone),
            "a label conclusively absent from static + dynamic is gone",
        );
    }

    #[test]
    fn deliver_worker_cron_leaves_it_when_the_owner_check_cannot_read() {
        // No db installed, so the durable-worker lookup fails: absence can't
        // be confirmed, so the cron must be left (retried next tick), not
        // deleted as owner-gone.
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project_with_static_workers("proj", "/tmp/wc-unknown", &[]);
        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, "proj", Some("scratch"), "x".to_owned(), false);
        assert!(
            matches!(outcome, crate::spawn::CronFireOutcome::DispatchFailed),
            "a failed owner check leaves the cron for the next tick, not TargetGone",
        );
    }

    #[test]
    fn deliver_cron_marks_an_overdue_fire_as_missed() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_static_workers("proj", "/tmp/wc-missed", &[]);
        let cwd = project_expanded_path(&ws, "proj");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionKey::from_session_id("lead-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                #[cfg(test)]
                account: AccountKey("test".to_owned()),
            },
        );

        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_cron_prompt(&ws, "proj", None, "standup".to_owned(), true);
        crate::spawn::deliver_cron_prompt(&ws, "proj", None, "standup".to_owned(), false);
        let dispatched = ws.drain_test_dispatch_buffer();
        let texts: Vec<String> = dispatched
            .iter()
            .filter_map(|c| match c {
                Command::Prompt { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.contains(&"[missed cron] standup".to_owned()),
            "an overdue fire is delivered with the missed marker",
        );
        assert!(texts.contains(&"standup".to_owned()), "an on-time fire has no marker");
    }

    #[test]
    fn fire_due_crons_missed_threshold_is_two_ticks() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_static_workers("proj", "/tmp/wc-thresh", &[]);
        let cwd = project_expanded_path(&ws, "proj");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionKey::from_session_id("lead-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                #[cfg(test)]
                account: AccountKey("test".to_owned()),
            },
        );
        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");

        let now = std::time::SystemTime::now();
        let cron = |id: &str, prompt: &str, next_fire| CronEntry {
            id: CronId::from(id),
            project_name: "proj".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: prompt.to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire,
            team_role: None,
        };
        // Due one tick ago: within the jitter window, on-time. Due three
        // ticks ago: a genuine catch-up, missed.
        ws.push_cron(cron("recent", "recent", now - std::time::Duration::from_secs(60)));
        ws.push_cron(cron("stale", "stale", now - std::time::Duration::from_secs(180)));

        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);
        let texts: Vec<String> = ws
            .drain_test_dispatch_buffer()
            .iter()
            .filter_map(|c| match c {
                Command::Prompt { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"recent".to_owned()), "a one-tick-late fire is on-time");
        assert!(
            texts.contains(&"[missed cron] stale".to_owned()),
            "a three-tick-late fire is marked missed",
        );
    }

    #[test]
    fn fire_due_crons_removes_a_cron_whose_project_is_gone() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        // No db installed, so push_cron stays in memory - no config dir needed.
        let (ws, _rx) = Workspace::testing_stub();
        let now = std::time::SystemTime::now();

        ws.push_cron(CronEntry {
            id: CronId::from("orphan"),
            project_name: "deleted-project".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH, // overdue -> due now
            team_role: None,
        });

        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);
        let dispatched = ws.drain_test_dispatch_buffer();

        assert!(
            dispatched.iter().all(|c| !matches!(c, crate::protocol::Command::SpawnProject { .. })),
            "a cron whose project is gone gets no spawn",
        );
        assert!(
            ws.crons_for_project("deleted-project").is_empty(),
            "an overdue cron whose project left forge.toml is removed, not advanced forever",
        );
    }

    #[test]
    fn advance_or_remove_cron_removes_never_occurring_recurring() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        // "0 0 30 2 *" (Feb 30) has no upcoming occurrence, so
        // next_fire_after returns None and the entry is removed (with a
        // warn) rather than left stuck at a stale next_fire.
        ws.push_cron(CronEntry {
            id: CronId::from("impossible"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 0 30 2 *".to_owned()),
            prompt: "p".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        });
        ws.advance_or_remove_cron(&CronId::from("impossible"), std::time::SystemTime::now());
        assert!(
            ws.crons_for_project("forge").is_empty(),
            "a recurring cron with no upcoming occurrence is removed, not left stuck",
        );
    }

    fn make_workspace_dir() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    #[tokio::test]
    async fn new_for_test_opens_redb_under_the_tempdir() {
        let dir = make_workspace_dir();
        let _workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        // The test constructor redirects redb into the config dir's own
        // tempdir, so no test ever opens the real machine store (#392).
        let redirected = dir.path().join("app-support").join("db.redb");
        assert!(redirected.exists(), "new_for_test opens redb under the tempdir app-support base");
    }

    #[tokio::test]
    async fn get_agent_handle_default_is_idempotent() {
        let dir = make_workspace_dir();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let settings = SessionLaunchSettings::default();

        let handle1 =
            workspace.get_agent_handle(SessionTarget::Default, settings.clone()).expect("first");
        let handle2 = workspace.get_agent_handle(SessionTarget::Default, settings).expect("second");

        assert!(Arc::ptr_eq(&handle1, &handle2), "expected pool hit for repeated Default target");
        assert_eq!(workspace.pool.lock().len(), 1);
    }

    #[tokio::test]
    async fn distinct_targets_pool_distinct_entries() {
        let dir = make_workspace_dir();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let settings = SessionLaunchSettings::default();

        let _ =
            workspace.get_agent_handle(SessionTarget::Default, settings.clone()).expect("default");
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, settings.clone())
            .expect("default again");
        assert_eq!(workspace.pool.lock().len(), 1, "Default is idempotent");

        let other = SessionKey::from_str_for_test("dual-test-other");
        let _ =
            workspace.get_agent_handle(SessionTarget::Session(other), settings).expect("session");
        assert_eq!(workspace.pool.lock().len(), 2, "distinct target adds a pool entry");
    }

    #[tokio::test]
    async fn shutdown_drains_pool() {
        let dir = make_workspace_dir();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let handle = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
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
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[orgs.projects]]
name = "dotfiles"
path = "~/Projects/dotfiles"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");

        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .expect("default");
        let _ = workspace
            .get_agent_handle(
                SessionTarget::Named("dotfiles".to_owned()),
                SessionLaunchSettings::default(),
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
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");

        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let result = workspace.get_agent_handle(
            SessionTarget::Named("nonexistent".to_owned()),
            SessionLaunchSettings::default(),
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
accounts = ["Subspace", "Granite"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "~/.claude-granite"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    #[tokio::test]
    async fn pool_records_picked_account() {
        let dir = make_workspace_dir_with_two_accounts();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .expect("default");
        let bound = workspace.pool.lock().values().map(|p| p.account.0.clone()).collect::<Vec<_>>();
        assert_eq!(bound.len(), 1);
        // Cold cache → unknown-first tie-break is the project's
        // `accounts = ["Subspace", "Granite"]` order. Subspace wins.
        assert_eq!(bound[0], "Subspace");
    }

    #[tokio::test]
    async fn cold_cache_spawns_rotate_across_allow_list() {
        // Two healthy accounts in the allow-list, two spawns. Round-
        // robin cursor advances per pick, so the first spawn lands
        // on the first allow-list entry (Subspace) and the second
        // rotates to Granite. Cursor is shared across the workspace
        // so even cold-cache spawns spread load rather than always
        // hammering the first account.
        let dir = make_workspace_dir_with_two_accounts();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

        let _ = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .expect("first");

        let other = SessionKey::from_str_for_test("dual-account-test-other");
        let _ = workspace
            .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
            .expect("second");

        let mut bound =
            workspace.pool.lock().values().map(|p| p.account.0.clone()).collect::<Vec<_>>();
        bound.sort();
        assert_eq!(
            bound,
            vec!["Granite".to_owned(), "Subspace".to_owned()],
            "two spawns must split across the two healthy accounts (round-robin)",
        );
    }

    #[tokio::test]
    async fn project_account_pin_excludes_unpinned_account() {
        // Three accounts globally; default org pins only
        // {Subspace, Granite}. Spawn under the default project picks
        // one of the pinned pair (Granite via alpha tie-break) and
        // must never touch Personal. Multi-spawn rotation within the
        // subset is exercised by the unit tests in `account.rs`
        // (`lru_restricted_pool_lru_within_subset`,
        // `round_robin_restricted_pool_cycles_within_subset`).
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Subspace", "Granite"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "~/.claude-granite"

[[accounts]]
display_name = "Personal"
config_dir = "~/.claude-personal"
"#,
        )
        .expect("write forge.toml");

        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let _ = workspace
            .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
            .expect("default spawn");

        let bound = workspace.pool.lock().values().map(|p| p.account.0.clone()).collect::<Vec<_>>();
        assert_eq!(bound.len(), 1);
        assert!(
            bound[0] == "Subspace" || bound[0] == "Granite",
            "spawn must land on a pinned account, got {bound:?}",
        );
        assert_ne!(bound[0], "Personal", "Personal must be excluded by the pin");
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
    -> (Arc<Workspace>, mpsc::UnboundedReceiver<forge_primitives::AgentCommand>, SessionKey) {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionKey::from_str_for_test("refresh-test");
        let rx = workspace.install_testing_stub(&key);
        // Stamp a session_id so refresh paths don't bail on
        // "no session_id yet".
        if let Some(domain) = workspace.domain_session_for(&key) {
            domain.lock().session_id =
                Some(forge_primitives::SessionId::new(key.as_str().to_owned()));
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
        let key = SessionKey::from_str_for_test("never-registered");
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

    // ---- Session-task rekey tests ----
    //
    // Pin dispatch routing across session-task key migration. The
    // `SessionTask` is registered under the pool key from
    // `resolve_target`, but TUI's `active_session_key` flips to the
    // real session UUID on `SessionUpdate::SessionReplaced`. Without
    // `migrate_session_task`, `Command::Prompt { key: new_uuid }`
    // falls off `dispatch`'s key
    // lookup with `UnknownSession`. These tests pin the routing.

    /// Seed `command_senders`, `pool`, and `domain_handles` at `key`
    /// against a fresh stub handle so `Workspace::dispatch` will
    /// route through the `SessionTask`-style fast path (rather than
    /// the test-only direct fallback). Returns the routed-command
    /// receiver so the test can assert on what flows through.
    fn install_fake_session_task(
        workspace: &Arc<Workspace>,
        key: &SessionKey,
    ) -> mpsc::UnboundedReceiver<Command> {
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        let arc = Arc::new(handle);
        workspace.pool.lock().insert(
            key.clone(),
            PooledAgent {
                handle: Arc::clone(&arc),
                #[cfg(test)]
                account: AccountKey("test".to_owned()),
            },
        );
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        workspace.command_senders.lock().insert(key.clone(), cmd_tx);
        let domain = workspace.register_domain_session(key.clone(), Some(arc));
        domain.lock().session_id = Some(forge_primitives::SessionId::new(key.as_str()));
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
        let key = SessionKey::from_str_for_test("lead-uuid");
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
            spawn_key: None,
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

    /// Direct unit test on `migrate_session_task`: each map (`pool`,
    /// `command_senders`, `domain_handles`) moves from `from` to `to`,
    /// and the migrated `DomainSession.key` field is rewritten.
    #[test]
    fn migrate_session_task_moves_all_three_maps() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let from = SessionKey::from_str_for_test("old-pool-key");
        let to = SessionKey::from_str_for_test("real-session-uuid");
        let _cmd_rx = install_fake_session_task(&workspace, &from);

        assert!(workspace.command_senders.lock().contains_key(&from));
        assert!(workspace.pool.lock().contains_key(&from));
        assert!(workspace.domain_handles.lock().contains_key(&from));

        workspace.migrate_session_task(&from, &to);

        assert!(!workspace.command_senders.lock().contains_key(&from));
        assert!(workspace.command_senders.lock().contains_key(&to));
        assert!(!workspace.pool.lock().contains_key(&from));
        assert!(workspace.pool.lock().contains_key(&to));
        assert!(!workspace.domain_handles.lock().contains_key(&from));
        let domain = workspace.domain_handles.lock().get(&to).cloned().expect("migrated");
        assert_eq!(domain.lock().key.as_str(), to.as_str());
    }

    /// Regression for the `/new` prompt-stuck bug. A `Command::Prompt`
    /// dispatched against the new key must route through the
    /// migrated `command_senders` entry, not fall off with
    /// `UnknownSession`. Mirrors what happens inside the SessionTask
    /// after the second `AgentEvent::Connected` (the `/new` path).
    #[test]
    fn dispatch_after_migrate_routes_to_new_key() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let from = SessionKey::from_str_for_test("pre-new-session");
        let to = SessionKey::from_str_for_test("post-new-session");
        let mut cmd_rx = install_fake_session_task(&workspace, &from);

        // Before migrate: dispatch at `to` fails because no entry
        // exists, dispatch at `from` succeeds.
        let result = workspace.dispatch(Command::Cancel { key: to.clone() });
        assert!(matches!(result, Err(DispatchError::UnknownSession(_))));

        workspace.dispatch(Command::Cancel { key: from.clone() }).expect("from routes");
        assert!(matches!(cmd_rx.try_recv(), Ok(Command::Cancel { .. })));

        // Migrate, then re-test.
        workspace.migrate_session_task(&from, &to);

        let result = workspace.dispatch(Command::Cancel { key: from.clone() });
        assert!(
            matches!(result, Err(DispatchError::UnknownSession(_))),
            "old key must not route after migration"
        );

        workspace.dispatch(Command::Cancel { key: to }).expect("new key routes");
        let cmd = cmd_rx.try_recv().expect("queued on migrated channel");
        assert!(matches!(cmd, Command::Cancel { .. }));
    }

    /// `migrate_session_task` with `from == to` is a no-op. Guards
    /// the typical case where the pool key already equals the real
    /// session UUID (e.g., resuming an existing lead session).
    #[test]
    fn migrate_session_task_no_op_when_from_equals_to() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let key = SessionKey::from_str_for_test("same-key");
        let _cmd_rx = install_fake_session_task(&workspace, &key);

        workspace.migrate_session_task(&key, &key);

        assert!(workspace.command_senders.lock().contains_key(&key));
        assert!(workspace.pool.lock().contains_key(&key));
        assert!(workspace.domain_handles.lock().contains_key(&key));
    }

    /// `migrate_session_task` on an unregistered source is a no-op.
    #[test]
    fn migrate_session_task_no_op_when_from_unregistered() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let from = SessionKey::from_str_for_test("never-registered");
        let to = SessionKey::from_str_for_test("destination");

        workspace.migrate_session_task(&from, &to);

        assert!(!workspace.command_senders.lock().contains_key(&from));
        assert!(!workspace.command_senders.lock().contains_key(&to));
        assert!(!workspace.pool.lock().contains_key(&to));
        assert!(workspace.domain_session_for(&to).is_none());
    }

    /// A rekey must carry peer badges + open-ask keys to the new key:
    /// `peer_stats` moves off `from`, and every `inflight_asks` entry
    /// keyed on `from` (as caller or stamped target_session) is
    /// rewritten to `to` so replies + expiry hit the live key.
    #[test]
    fn migrate_session_task_moves_peer_stats_and_open_ask_keys() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk};
        let (workspace, _update_rx) = Workspace::testing_stub();
        let from = SessionKey::from_str_for_test("synth-key");
        let to = SessionKey::from_str_for_test("real-uuid");
        let _cmd_rx = install_fake_session_task(&workspace, &from);

        workspace.peer_stats.lock().entry(from.clone()).or_default().incoming = 1;

        let as_caller = CorrelationId::new_ask();
        let as_target = CorrelationId::new_ask();
        {
            let mut asks = workspace.inflight_asks.lock();
            asks.insert(
                as_caller.clone(),
                InflightAsk {
                    correlation_id: as_caller.clone(),
                    channel: crate::mcp::peers::types::AskChannel::Peers,
                    caller: from.clone(),
                    target_project: "granite-backend".to_owned(),
                    target_session: None,
                },
            );
            asks.insert(
                as_target.clone(),
                InflightAsk {
                    correlation_id: as_target.clone(),
                    channel: crate::mcp::peers::types::AskChannel::Peers,
                    caller: SessionKey::from_str_for_test("someone-else"),
                    target_project: "forge".to_owned(),
                    target_session: Some(from.clone()),
                },
            );
        }

        assert!(workspace.migrate_session_task(&from, &to));

        {
            let stats = workspace.peer_stats.lock();
            assert_eq!(stats.get(&to).map(|s| s.incoming), Some(1), "badge follows the session");
            assert!(!stats.contains_key(&from), "stale key dropped");
        }
        {
            let asks = workspace.inflight_asks.lock();
            assert_eq!(
                asks.get(&as_caller).map(|a| a.caller.clone()),
                Some(to.clone()),
                "caller rekeyed to the live session",
            );
            assert_eq!(
                asks.get(&as_target).and_then(|a| a.target_session.clone()),
                Some(to.clone()),
                "target_session rekeyed to the live session",
            );
        }
    }

    /// When `to` already carries peer counts (a lingering resumed
    /// UUID), migrate MERGES `from`'s counts in rather than clobbering
    /// `to` or dropping `from` - erring toward keeping counts.
    #[test]
    fn migrate_session_task_merges_peer_stats_into_existing_to() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let from = SessionKey::from_str_for_test("synth-key");
        let to = SessionKey::from_str_for_test("real-uuid");
        let _cmd_rx = install_fake_session_task(&workspace, &from);

        {
            let mut stats = workspace.peer_stats.lock();
            stats.entry(from.clone()).or_default().outgoing = 2;
            stats.entry(to.clone()).or_default().outgoing = 3;
        }

        assert!(workspace.migrate_session_task(&from, &to));

        let stats = workspace.peer_stats.lock();
        assert_eq!(stats.get(&to).map(|s| s.outgoing), Some(5), "counts merge, not clobber");
        assert!(!stats.contains_key(&from), "stale key dropped after merge");
    }

    /// `classify_oauth_usage_error` must distinguish HTTP 429 from
    /// auth-related failures so the TUI's bottom-panel hint reads
    /// `rate-limited` (the common case under multiple forge
    /// instances) rather than collapsing every failure to a single
    /// generic bucket.
    #[test]
    fn classify_oauth_usage_error_buckets_known_variants() {
        use crate::account::UsageFetchStatus;
        use forge_primitives::usage::oauth::OauthUsageError;

        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::HttpStatus(429, String::new())),
            UsageFetchStatus::RateLimited,
        );
        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(60)),
            }),
            UsageFetchStatus::RateLimited,
            "new dedicated 429 variant also maps to RateLimited",
        );
        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::RateLimited { retry_after: None }),
            UsageFetchStatus::RateLimited,
        );
        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::Unauthorized(401)),
            UsageFetchStatus::Unauthorized,
        );
        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::Expired),
            UsageFetchStatus::Expired,
        );
        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::NoCredentials),
            UsageFetchStatus::Expired,
        );
        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::Network("dns".to_owned())),
            UsageFetchStatus::NetworkFailed,
        );
        // Non-429 HTTP errors and decode failures fall through to the
        // generic `Other` bucket - renderers show "fetch failed" so
        // the user can tell something's wrong without naming a cause.
        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::HttpStatus(500, String::new())),
            UsageFetchStatus::Other,
        );
        assert_eq!(
            classify_oauth_usage_error(&OauthUsageError::Decode("bad json".to_owned())),
            UsageFetchStatus::Other,
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
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[orgs.projects]]
name = "granite-backend"
path = "~/Projects/granite-backend"
auto_start = false

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
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
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

        let caller = SessionKey::from_str_for_test("caller-1");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: caller.clone(),
                target_project: "granite-backend".to_owned(),
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
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionKey::from_str_for_test("caller-notice");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: caller.clone(),
                target_project: "granite-backend".to_owned(),
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
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionKey::from_str_for_test("caller-notice-echo");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: AskChannel::Peers,
                caller: caller.clone(),
                target_project: "granite-backend".to_owned(),
                target_session: None,
            },
        );

        workspace.expire_inflight_ask_failed(&id, PeerFailureReason::TargetConnectionFailed);

        let mut echo = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PeerEnvelopeAppended { session_id, wrapped } = update {
                echo = Some((session_id, wrapped));
            }
        }
        let (session_id, wrapped) = echo.expect("PeerEnvelopeAppended painted for the caller");
        assert_eq!(session_id, caller.as_str(), "notice echo targets the caller's session");
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
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

        let worker_key = SessionKey::from_str_for_test("worker-sess-1");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Workers,
                caller: SessionKey::from_str_for_test("lead-1"),
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

    /// A peer ask buffered against a `__spawn_<project>__` synth key
    /// whose spawn never connects must be failed: the ask was never
    /// delivered (no target_session stamp) and the synth key resolves
    /// to no catalog project, so only expire_buffered_peer_prompts
    /// can reach it.
    #[tokio::test]
    async fn expire_buffered_peer_prompts_fails_undelivered_spawn_asks() {
        use crate::domain_session::DomainSession;
        use crate::mcp::peers::types::{
            AskChannel, CorrelationId, InflightAsk, PeerFailureReason, WrappedKind, WrappedPrompt,
        };
        let (workspace, _rx) = Workspace::testing_stub();

        let synth_key = SessionKey::from_session_id("__spawn_granite-backend__");
        let id = CorrelationId::new_ask();
        let wrapped = WrappedPrompt {
            correlation_id: id.clone(),
            kind: WrappedKind::Question,
            channel: AskChannel::Peers,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            body: "are you up?".to_owned(),
        };
        let domain = Arc::new(Mutex::new(DomainSession::new(synth_key.clone(), None)));
        domain.lock().pending_peer_prompts.push(wrapped);
        workspace.domain_handles.lock().insert(synth_key.clone(), domain);
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: SessionKey::from_str_for_test("asker"),
                target_project: "granite-backend".to_owned(),
                target_session: None,
            },
        );

        workspace
            .expire_buffered_peer_prompts(&synth_key, PeerFailureReason::TargetConnectionFailed);

        assert!(
            !workspace.inflight_asks.lock().contains_key(&id),
            "buffered ask failed when the spawn never connected"
        );
        assert!(
            workspace
                .domain_handles
                .lock()
                .get(&synth_key)
                .unwrap()
                .lock()
                .pending_peer_prompts
                .is_empty(),
            "buffered prompts drained"
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
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

        let caller = SessionKey::from_str_for_test("asker");
        let target = SessionKey::from_str_for_test("replier");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: caller.clone(),
                target_project: "granite-backend".to_owned(),
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
        let target = SessionKey::from_str_for_test("replier");
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Peers,
                caller: SessionKey::from_str_for_test("asker"),
                target_project: "granite-backend".to_owned(),
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
    async fn dispatch_command_deliver_peer_prompt_routes_cleanly() {
        use crate::mcp::peers::types::{AskChannel, CorrelationId, WrappedKind, WrappedPrompt};
        let dir = forge_toml_with_two_projects();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

        let caller = SessionKey::from_str_for_test("caller-dispatch");
        let wrapped = WrappedPrompt {
            correlation_id: CorrelationId::new_tell(),
            kind: WrappedKind::Message,
            channel: AskChannel::Peers,
            sender_name: "forge".to_owned(),
            sender_org: "Default".to_owned(),
            body: "fyi".to_owned(),
        };
        // The command channel is the workspace's main dispatch bus -
        // routing to an unknown target still queues, the spawn handler
        // is the one that rejects. Smoke: dispatch returns Ok.
        let result = workspace.dispatch(crate::protocol::Command::DeliverPeerPrompt {
            caller,
            target_project: "granite-backend".to_owned(),
            wrapped,
        });
        assert!(result.is_ok(), "dispatch routed cleanly: {result:?}");
    }

    #[tokio::test]
    async fn deliver_reply_to_caller_routes_by_session_and_guards() {
        use crate::mcp::peers::facade::ReplyDeliverError;
        use crate::mcp::peers::types::{AskChannel, CorrelationId, WrappedKind, WrappedPrompt};
        let (ws, _rx) = Workspace::testing_stub();
        ws.enable_test_dispatch_intercept();

        let caller = SessionKey::from_str_for_test("asker");
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
            PooledAgent { handle: Arc::new(handle), account: AccountKey("acct".to_owned()) },
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
        let ghost = SessionKey::from_str_for_test("ghost");
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

        let caller = SessionKey::from_str_for_test("asker");
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
            PooledAgent { handle: Arc::new(handle), account: AccountKey("acct".to_owned()) },
        );
        assert_eq!(ws.deliver_reply_to_caller(&caller, &reply), Ok(()));

        let mut echo = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PeerEnvelopeAppended { session_id, wrapped } = update {
                echo = Some((session_id, wrapped));
            }
        }
        let (session_id, wrapped) = echo.expect("PeerEnvelopeAppended painted for the caller");
        assert_eq!(session_id, caller.as_str(), "echo targets the caller's session");
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
    async fn peer_mcp_workspace_fixture() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = forge_toml_with_two_projects();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
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
    /// resolves the closing session's project via `list_projects()`
    /// (catalog-backed); a fully-in-memory test would early-return.
    #[tokio::test]
    async fn expire_target_inflight_drains_only_targeted_asks() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk, PeerFailureReason};

        let (workspace, _dir) = peer_mcp_workspace_fixture().await;

        // Seed catalog so list_projects() sees a session under
        // "granite-backend". The session_id is what we'll feed to
        // expire_target_inflight as the closing key.
        let granite_cwd = project_expanded_path(&workspace, "granite-backend");
        let target_session_id = "target-session-uuid";
        workspace.record_connected_session(&granite_cwd, target_session_id, None);

        // Three inflight asks: two targeting granite-backend (must
        // expire), one targeting forge (must survive).
        let caller_a = SessionKey::from_str_for_test("caller-a");
        let caller_b = SessionKey::from_str_for_test("caller-b");
        let caller_c = SessionKey::from_str_for_test("caller-c");
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
                    target_project: "granite-backend".to_owned(),
                    target_session: None,
                },
            );
            asks.insert(
                id_b.clone(),
                InflightAsk {
                    correlation_id: id_b.clone(),
                    channel: crate::mcp::peers::types::AskChannel::Peers,
                    caller: caller_b.clone(),
                    target_project: "granite-backend".to_owned(),
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
        let closing_key = SessionKey::from_session_id(target_session_id);
        workspace.expire_target_inflight(&closing_key, PeerFailureReason::TargetConnectionFailed);

        // Targeted asks are gone; the orthogonally-targeted ask survives.
        let asks = workspace.inflight_asks.lock();
        assert!(!asks.contains_key(&id_a), "ask targeting granite-backend removed");
        assert!(!asks.contains_key(&id_b), "ask targeting granite-backend removed");
        assert!(
            asks.contains_key(&id_c),
            "ask targeting forge survives, only the closing project's asks expire"
        );
        drop(asks);

        // One DeliveryFailureNotice Command::Prompt per expired ask,
        // routed back to each ask's caller. Sort by caller key before
        // comparing; HashMap iteration order isn't pinned.
        let buffered = workspace.drain_test_dispatch_buffer();
        let mut notice_callers: Vec<SessionKey> = buffered
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
        notice_callers.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut expected_callers = vec![caller_a.clone(), caller_b.clone()];
        expected_callers.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            notice_callers, expected_callers,
            "DeliveryFailureNotice fired for exactly the two granite-backend-targeted callers"
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
            session_key: SessionKey::from_session_id(key),
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".into(),
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
        assert_eq!(removed.unwrap().session_key.as_str(), "new");
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
            ws.insert_live_worker_if_label_absent(&project, fake_entry("reviewer", "first"))
                .is_ok(),
            "the first insert for a label wins",
        );
        let existing = ws
            .insert_live_worker_if_label_absent(&project, fake_entry("reviewer", "second"))
            .expect_err("a second live worker for the same label is rejected");
        assert_eq!(existing.as_str(), "first", "the live holder is returned");
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
            ws.insert_live_worker_if_label_absent(&project, fake_entry("reviewer", "fresh"))
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

    /// `migrate_session_task` rewrites every matching WorkerEntry's
    /// `session_key` field in lockstep with the pool / command_senders
    /// / domain_handles maps. Without this fix-up, `close_worker` and
    /// `deliver_worker_prompt` would address the wrong session after
    /// the synth -> real rekey on first Connected.
    #[test]
    fn migrate_session_task_rekeys_live_workers() {
        let (ws, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        let from = SessionKey::from_session_id("__spawn_worker_forge_r__");
        let to = SessionKey::from_session_id("real-uuid");
        ws.insert_live_worker(&project, fake_entry("r", from.as_str()));
        // Seed the three maps at `from` so migrate doesn't short-circuit
        // on the "not registered" branch.
        ws.command_senders
            .lock()
            .insert(from.clone(), tokio::sync::mpsc::unbounded_channel::<Command>().0);
        ws.register_domain_session(from.clone(), None);

        let migrated = ws.migrate_session_task(&from, &to);
        assert!(migrated, "migrate succeeded");
        let entries = ws.list_live_workers(&project);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_key.as_str(), "real-uuid");
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

    fn fake_entry(label: &str, key: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.into(),
            charter: "test charter".into(),
            session_key: SessionKey::from_session_id(key),
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".into(),
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
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// `release_session_with_cascade` cascades worker termination when
    /// the released session is a project's lead. Workers' JSONLs persist on disk
    /// (we don't delete them); only the in-memory live_workers
    /// entries + the running session subprocesses are torn down.
    #[tokio::test]
    async fn release_session_on_lead_cascades_workers() {
        let dir = make_workspace_dir();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        // Seed the catalog with a lead session at the project key so
        // `list_projects` reports a session matching `lead_key`.
        let project = workspace.list_projects().into_iter().next().expect("forge project");
        let project_key = project.key.clone();
        let lead_key = SessionKey::from_session_id("lead-uuid");
        workspace.record_connected_session(
            &project.path.to_string_lossy(),
            lead_key.as_str(),
            None,
        );

        // Insert two workers under the project.
        workspace.insert_live_worker(&project_key, fake_entry("r1", "worker-1"));
        workspace.insert_live_worker(&project_key, fake_entry("r2", "worker-2"));
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
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

        let project_key = workspace.list_projects().into_iter().next().expect("forge").key;
        workspace.insert_live_worker(&project_key, fake_entry("r1", "worker-1"));

        let unknown = SessionKey::from_session_id("unknown-session");
        workspace.release_session_with_cascade(&unknown);
        assert_eq!(
            workspace.list_live_workers(&project_key).len(),
            1,
            "non-lead release must not cascade"
        );
    }

    /// Regression for C1: when a worker's Connected fires it lands
    /// at `catalog[project][0]` via `record_connected_session`. If
    /// cascade detection uses `sessions.first()` as the lead marker,
    /// it picks up the worker (not the lead) and releasing the real
    /// lead silently stops cascading. The fix consults
    /// `live_workers` to discriminate: a session is a lead iff it
    /// appears in the catalog AND not in live_workers.
    #[tokio::test]
    async fn release_session_cascades_when_worker_sits_at_catalog_head() {
        let dir = make_workspace_dir();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

        let project = workspace.list_projects().into_iter().next().expect("forge project");
        let project_key = project.key.clone();
        let project_path = project.path.to_string_lossy().into_owned();

        // Seed the lead FIRST, then the worker. record_connected_session
        // inserts at index 0, so after both calls catalog[0] is the
        // worker and catalog[1] is the lead - the exact shape that
        // breaks the old `sessions.first()` discriminator.
        let lead_key = SessionKey::from_session_id("lead-uuid");
        let worker_key = SessionKey::from_session_id("worker-uuid");
        workspace.record_connected_session(&project_path, lead_key.as_str(), None);
        workspace.record_connected_session(&project_path, worker_key.as_str(), None);
        workspace.insert_live_worker(&project_key, fake_entry("reviewer", worker_key.as_str()));

        // Sanity: catalog head is the worker, not the lead.
        let projects_now = workspace.list_projects();
        let first_session = &projects_now[0].sessions[0].session;
        assert_eq!(
            first_session, &worker_key,
            "catalog[0] must be the worker for the regression to bite"
        );

        // Release the LEAD. Pre-fix this fell through to a no-op
        // because the cascade check (`sessions.first() == lead_key`)
        // failed - the head was the worker.
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

    fn fake_spawning_entry(
        label: &str,
        key: &str,
        needs_tag: bool,
    ) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.into(),
            charter: "test".into(),
            session_key: SessionKey::from_session_id(key),
            status: forge_primitives::WorkerLiveness::Spawning,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead".into(),
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
    #[tokio::test]
    async fn notfound_keeps_worker_with_needs_tag_flag() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("forge");
        let session_id = "550e8400-e29b-41d4-a716-446655440010";
        let session_key = SessionKey::from_session_id(session_id);
        workspace.insert_live_worker(&project_key, fake_spawning_entry("idle", session_id, true));

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
            &project_key,
            "idle",
            &cwd.path().to_string_lossy(),
            false,
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
    }

    /// `apply_worker_tag_or_rollback_with_config_dir`: when the JSONL
    /// exists from the start, the retry succeeds on the first attempt,
    /// the worker transitions to Running, and `needs_tag` is cleared.
    #[tokio::test]
    async fn jsonl_present_clears_needs_tag() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("forge");
        let session_id = "550e8400-e29b-41d4-a716-446655440011";
        let session_key = SessionKey::from_session_id(session_id);
        workspace.insert_live_worker(
            &project_key,
            fake_spawning_entry("prompt-driven", session_id, true),
        );

        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let path = jsonl_path_for(cfg.path(), cwd.path(), session_id);
        fs::write(&path, "").expect("seed jsonl");

        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &session_key,
            &project_key,
            "prompt-driven",
            &cwd.path().to_string_lossy(),
            false,
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
        let session_key = SessionKey::from_session_id(session_id);
        // Pre-state: worker is Running but needs_tag=true (it landed
        // here via the deferred-NotFound branch earlier).
        let mut entry = fake_spawning_entry("idle", session_id, true);
        entry.status = forge_primitives::WorkerLiveness::Running;
        workspace.insert_live_worker(&project_key, entry);

        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let path = jsonl_path_for(cfg.path(), cwd.path(), session_id);
        fs::write(&path, "").expect("seed jsonl (claude has now written one)");

        workspace.retry_worker_tag_opportunistic_with_config_dir(
            &project_key,
            &session_key,
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
    /// resume scan from #157/#164 picks the orphaned pre-/new
    /// JSONL on the next forge restart.
    ///
    /// Workspace-level test: simulate the /new flow by calling the
    /// tagger twice, migrating the WorkerEntry's session_key between
    /// calls (the production `migrate_session_task` does this on
    /// rekey_to). Verify both session_ids' JSONLs land with the tag.
    #[tokio::test]
    async fn worker_tag_re_applied_after_new_session_rekey() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("forge");

        // Two distinct session_ids: first Connected, then post-/new.
        let session_id_1 = "550e8400-e29b-41d4-a716-446655440021";
        let session_id_2 = "550e8400-e29b-41d4-a716-446655440022";
        let key_1 = SessionKey::from_session_id(session_id_1);
        let key_2 = SessionKey::from_session_id(session_id_2);

        // Seed the WorkerEntry with the FIRST session's key. The
        // production flow inserts at the synth key and migrates to
        // the real key on first Connected; for this test the entry
        // is already at the first real key.
        workspace
            .insert_live_worker(&project_key, fake_spawning_entry("reviewer", session_id_1, true));

        let cfg = tempdir().expect("cfg");
        let cwd = tempdir().expect("cwd");
        let path_1 = jsonl_path_for(cfg.path(), cwd.path(), session_id_1);
        fs::write(&path_1, "").expect("seed first jsonl");

        // First Connected: tag the first session's JSONL.
        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &key_1,
            &project_key,
            "reviewer",
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
        .expect("first StatusChanged within budget");
        let body_1 = fs::read_to_string(&path_1).expect("read first jsonl");
        assert!(
            body_1.contains("\"tag\":\"forge:worker:reviewer\""),
            "first Connected tags the first session's JSONL",
        );

        // Simulate /new's rekey: the WorkerEntry's session_key
        // migrates to the new real session_id. Production does this
        // via `migrate_session_task` from `rekey_to`.
        assert!(
            workspace.migrate_session_task(&key_1, &key_2),
            "migrate worker entry to new session key",
        );

        // Seed the second session's JSONL as if claude wrote it on
        // the first turn after /new.
        let path_2 = jsonl_path_for(cfg.path(), cwd.path(), session_id_2);
        fs::write(&path_2, "").expect("seed second jsonl");

        // Second Connected (the /new flow). Without #166's fix the
        // tagger was never called; with the fix it fires
        // unconditionally on every Connected.
        workspace.apply_worker_tag_or_rollback_with_config_dir(
            &key_2,
            &project_key,
            "reviewer",
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
    /// catalog scan found zero tagged worker JSONLs and every role
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
        let session_key = SessionKey::from_session_id(session_id);

        // is_git_repo_at_spawn=true is the production shape that
        // triggers claude's --worktree fork.
        let mut entry = fake_spawning_entry("debugger", session_id, true);
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
            &project_key,
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
        let session_key = SessionKey::from_session_id(session_id);

        // Pre-state: worker is Running (the apply_* path already ran
        // and exhausted into DeferredNotFound). is_git_repo_at_spawn=true.
        let mut entry = fake_spawning_entry("debugger", session_id, true);
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
mod team_spawn_tests {
    use super::*;
    use crate::protocol::Command;
    use crate::team::Role;

    fn role(label: &str) -> Role {
        Role {
            label: label.to_owned(),
            charter: format!("test charter for {label}"),
            initial_kick: format!("test kick for {label}"),
        }
    }

    #[test]
    fn delegation_catalog_lists_globals_for_a_project() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("implementer");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("charter.md"), "description: Generic code-writer\n")
            .expect("write");
        let _guard = crate::team::override_forge_team_root_for_test(tmp.path().to_path_buf());

        let text = Workspace::build_delegation_catalog("forge");
        assert!(text.contains("workers__spawn"));
        assert!(text.contains("implementer - Generic code-writer"));
    }

    #[test]
    fn spawn_team_for_lead_dispatches_one_command_per_role() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let lead_sid = "lead-uuid";
        let project_key = ProjectKey::new("proj-x");
        let team = vec![role("planner"), role("reviewer"), role("tester")];

        workspace.spawn_team_for_lead_with_resume(
            lead_sid,
            &project_key,
            &team,
            &std::collections::HashMap::new(),
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 3, "one SpawnWorker per configured role");

        let mut labels: Vec<String> = Vec::new();
        let mut charters: Vec<String> = Vec::new();
        for cmd in dispatched {
            match cmd {
                Command::SpawnWorker {
                    label,
                    charter,
                    spawned_by_session_id,
                    project_key: pk,
                    ..
                } => {
                    assert_eq!(spawned_by_session_id, lead_sid);
                    assert_eq!(pk, project_key);
                    labels.push(label);
                    charters.push(charter);
                }
                other => panic!("expected SpawnWorker, got {other:?}"),
            }
        }
        labels.sort();
        assert_eq!(labels, vec!["planner", "reviewer", "tester"]);
        for c in charters {
            assert!(!c.trim().is_empty(), "role charter must be non-empty");
        }
    }

    #[test]
    fn team_spawn_resolves_bare_label_to_project_charter() {
        // Fixture: a project-scoped hub-modules/steward charter only.
        let tmp = tempfile::tempdir().expect("tmp");
        let steward = tmp.path().join("hub-modules").join("steward");
        std::fs::create_dir_all(&steward).expect("mkdir");
        std::fs::write(steward.join("charter.md"), "description: Hub steward\n").expect("charter");
        std::fs::write(steward.join("kick.md"), "go\n").expect("kick");
        let _guard = crate::team::override_forge_team_root_for_test(tmp.path().to_path_buf());

        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        // No tokio runtime in a plain #[test] -> the sync fallback runs
        // load_team_roles inline, so the bare label resolves here.
        workspace.spawn_team_for_lead_with_catalog_scan(
            "lead-uuid".to_owned(),
            ProjectKey::new("hub-modules"),
            std::path::PathBuf::from("/tmp/hub-modules"),
            "hub-modules".to_owned(),
            vec!["steward".to_owned()],
            false,
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "bare team label resolves + spawns one worker");
        if let Command::SpawnWorker { label, charter, .. } = spawns[0] {
            assert_eq!(label, "steward", "worker label stays BARE, not hub-modules/steward");
            assert!(charter.contains("Hub steward"), "charter loaded from the project-scoped dir");
        }
    }

    #[test]
    fn force_new_team_spawn_dispatches_workers_fresh() {
        // A force-new lead's team spawn skips the catalog resume scan,
        // so every worker dispatches with resume_existing = None. (The
        // resume mechanic itself is covered by
        // spawn_team_for_lead_with_resume_all_resume.)
        let tmp = tempfile::tempdir().expect("tmp");
        let steward = tmp.path().join("hub-modules").join("steward");
        std::fs::create_dir_all(&steward).expect("mkdir");
        std::fs::write(steward.join("charter.md"), "description: Hub steward\n").expect("charter");
        std::fs::write(steward.join("kick.md"), "go\n").expect("kick");
        let _guard = crate::team::override_forge_team_root_for_test(tmp.path().to_path_buf());

        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.spawn_team_for_lead_with_catalog_scan(
            "lead-uuid".to_owned(),
            ProjectKey::new("hub-modules"),
            std::path::PathBuf::from("/tmp/hub-modules"),
            "hub-modules".to_owned(),
            vec!["steward".to_owned()],
            true, // force_new: skip the resume scan
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "force-new still spawns the configured worker");
        if let Command::SpawnWorker { resume_existing, .. } = spawns[0] {
            assert!(resume_existing.is_none(), "force_new => worker spawns fresh (no resume)");
        }
    }

    #[test]
    fn spawn_team_for_lead_with_empty_team_dispatches_nothing() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        workspace.spawn_team_for_lead_with_resume(
            "lead-uuid",
            &ProjectKey::new("proj-x"),
            &Vec::<Role>::new(),
            &std::collections::HashMap::new(),
        );
        assert!(workspace.drain_test_dispatch_buffer().is_empty());
    }

    /// #157: all-fresh path - when no roles have a matching entry in
    /// `resume_map`, every dispatched `SpawnWorker` carries
    /// `resume_existing = None`.
    #[test]
    fn spawn_team_for_lead_with_resume_all_fresh() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let team = vec![role("planner"), role("reviewer")];
        workspace.spawn_team_for_lead_with_resume(
            "lead-uuid",
            &ProjectKey::new("proj-x"),
            &team,
            &std::collections::HashMap::new(),
        );
        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 2);
        for cmd in dispatched {
            match cmd {
                Command::SpawnWorker { resume_existing, .. } => {
                    assert!(resume_existing.is_none(), "all-fresh path passes None");
                }
                other => panic!("expected SpawnWorker, got {other:?}"),
            }
        }
    }

    /// #157: all-resume path - every role has a matching entry in
    /// `resume_map`, so every dispatched `SpawnWorker` carries
    /// `resume_existing = Some(<expected_session_id>)`.
    #[test]
    fn spawn_team_for_lead_with_resume_all_resume() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let team = vec![role("planner"), role("reviewer")];
        let mut resume_map = std::collections::HashMap::new();
        resume_map.insert("planner".to_owned(), "planner-uuid".to_owned());
        resume_map.insert("reviewer".to_owned(), "reviewer-uuid".to_owned());
        workspace.spawn_team_for_lead_with_resume(
            "lead-uuid",
            &ProjectKey::new("proj-x"),
            &team,
            &resume_map,
        );
        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 2);
        for cmd in dispatched {
            match cmd {
                Command::SpawnWorker { label, resume_existing, .. } => {
                    let expected = resume_map.get(&label).cloned();
                    assert_eq!(resume_existing, expected, "resume_existing matches map entry");
                }
                other => panic!("expected SpawnWorker, got {other:?}"),
            }
        }
    }

    /// #157: mixed path - one role resumes, one is fresh.
    #[test]
    fn spawn_team_for_lead_with_resume_mixed() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let team = vec![role("planner"), role("reviewer")];
        let mut resume_map = std::collections::HashMap::new();
        resume_map.insert("planner".to_owned(), "planner-uuid".to_owned());
        // reviewer absent: fresh-spawn.
        workspace.spawn_team_for_lead_with_resume(
            "lead-uuid",
            &ProjectKey::new("proj-x"),
            &team,
            &resume_map,
        );
        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 2);
        let mut planner_resume: Option<String> = None;
        let mut reviewer_resume: Option<String> = None;
        for cmd in dispatched {
            match cmd {
                Command::SpawnWorker { label, resume_existing, .. } => match label.as_str() {
                    "planner" => planner_resume = resume_existing,
                    "reviewer" => reviewer_resume = resume_existing,
                    other => panic!("unexpected label {other}"),
                },
                other => panic!("expected SpawnWorker, got {other:?}"),
            }
        }
        assert_eq!(planner_resume, Some("planner-uuid".to_owned()));
        assert!(reviewer_resume.is_none(), "reviewer not in map → fresh-spawn");
    }

    fn dyn_worker(label: &str, kick: Option<&str>) -> crate::store::dynamic_workers::DynamicWorker {
        crate::store::dynamic_workers::DynamicWorker {
            project_key: "proj-x".to_owned(),
            label: label.to_owned(),
            charter: format!("dynamic charter for {label}"),
            kick: kick.map(str::to_owned),
        }
    }

    /// Dynamic re-spawn mirrors the static resume mechanic: a worker with
    /// a catalog-tag match resumes and takes the forge restart note as its
    /// kick; one without re-delivers its stored kick.
    #[test]
    fn spawn_dynamic_workers_for_lead_resumes_or_redelivers_kick() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let project_key = ProjectKey::new("proj-x");
        let dynamic = vec![
            dyn_worker("reviewer", Some("original kick")),
            dyn_worker("scratch", Some("start scratch")),
            dyn_worker("idle", None),
        ];
        let mut resume_map = std::collections::HashMap::new();
        resume_map.insert("reviewer".to_owned(), "reviewer-uuid".to_owned());

        workspace.spawn_dynamic_workers_for_lead("new-lead", &project_key, &dynamic, &resume_map);

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 3, "one SpawnWorker per persisted dynamic worker");
        for cmd in dispatched {
            let Command::SpawnWorker {
                label,
                charter,
                resume_existing,
                kick,
                spawned_by_session_id,
                project_key: pk,
                ..
            } = cmd
            else {
                panic!("expected SpawnWorker");
            };
            assert_eq!(spawned_by_session_id, "new-lead", "re-parented to the current lead");
            assert_eq!(pk, project_key);
            assert_eq!(charter, format!("dynamic charter for {label}"), "charter from the DB row");
            match label.as_str() {
                "reviewer" => {
                    assert_eq!(
                        resume_existing.as_deref(),
                        Some("reviewer-uuid"),
                        "tag match resumes"
                    );
                    assert_eq!(
                        kick.as_deref(),
                        Some(DYNAMIC_WORKER_RESTART_NOTE),
                        "resume delivers the forge restart note, not the stored kick",
                    );
                }
                "scratch" => {
                    assert!(resume_existing.is_none(), "no tag -> fresh");
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

    /// A project with an EMPTY static roster but a persisted dynamic
    /// worker still re-spawns it on lead reconnect (the empty-team early
    /// return must not skip the DB set).
    #[test]
    fn catalog_scan_respawns_dynamic_worker_without_static_roster() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let dir = tempfile::tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let project_key = ProjectKey::new("hub-modules");
        let _ = workspace.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: "hub-modules".to_owned(),
            label: "scratch".to_owned(),
            charter: "resume the scratch task".to_owned(),
            kick: Some("go".to_owned()),
        });
        workspace.enable_test_dispatch_intercept();

        // No tokio runtime in a plain #[test] -> the sync fallback path
        // dispatches with an empty resume map.
        workspace.spawn_team_for_lead_with_catalog_scan(
            "lead-uuid".to_owned(),
            project_key,
            std::path::PathBuf::from("/tmp/hub-modules"),
            "hub-modules".to_owned(),
            Vec::new(),
            false,
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        let spawns: Vec<&Command> =
            dispatched.iter().filter(|c| matches!(c, Command::SpawnWorker { .. })).collect();
        assert_eq!(spawns.len(), 1, "the persisted dynamic worker re-spawns with no static roster");
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
        let project_key = ProjectKey::new("hub-modules");
        let _ = workspace.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: "hub-modules".to_owned(),
            label: "scratch".to_owned(),
            charter: "c".to_owned(),
            kick: None,
        });
        workspace.delete_dynamic_worker(&project_key, "scratch");
        workspace.enable_test_dispatch_intercept();

        workspace.spawn_team_for_lead_with_catalog_scan(
            "lead-uuid".to_owned(),
            project_key,
            std::path::PathBuf::from("/tmp/hub-modules"),
            "hub-modules".to_owned(),
            Vec::new(),
            false,
        );

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, Command::SpawnWorker { .. })),
            "a deleted dynamic worker must not re-spawn",
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod build_resume_map_tests {
    use super::*;

    fn mk_info(session_id: &str, cwd: Option<&str>, tag: Option<&str>) -> SDKSessionInfo {
        SDKSessionInfo {
            session_id: session_id.to_owned(),
            summary: "test".to_owned(),
            last_modified: 0,
            file_size: None,
            custom_title: None,
            first_prompt: None,
            git_branch: None,
            cwd: cwd.map(str::to_owned),
            storage_key: cwd
                .map(|c| forge_agent::userdata::catalog::scan::project_key_for_directory(Some(c)))
                .unwrap_or_default(),
            tag: tag.map(str::to_owned),
            created_at: None,
        }
    }

    /// Regression for the worktree-subdir resume miss on PR #164:
    /// workers spawned with `--worktree=<label>` `chdir` into
    /// `<project>/.claude/worktrees/<label>/` which is indexed under
    /// a SIBLING `<config_dir>/projects/<sanitize(worktree_path)>/`
    /// subdir. A `directory=Some(<project>)` scan misses them. Scoping
    /// each worker to its `worker_tag_dir` run-dir storage key catches
    /// them: every worktree session lives under that run dir's key.
    #[test]
    fn build_resume_map_finds_workers_in_worktree_subdirs() {
        let project_dir = std::path::Path::new("/Users/me/Projects/forge");
        let sessions = vec![
            mk_info(
                "lead-uuid",
                Some("/Users/me/Projects/forge"),
                Some(forge_primitives::FORGE_LEAD_TAG),
            ),
            mk_info(
                "planner-uuid",
                Some("/Users/me/Projects/forge/.claude/worktrees/planner"),
                Some("forge:worker:planner"),
            ),
            mk_info(
                "reviewer-uuid",
                Some("/Users/me/Projects/forge/.claude/worktrees/reviewer"),
                Some("forge:worker:reviewer"),
            ),
        ];
        let map = build_resume_map_from_sessions(&sessions, project_dir, true);
        assert_eq!(map.len(), 2, "only worker-tagged sessions land in the map");
        assert_eq!(map.get("planner"), Some(&"planner-uuid".to_owned()));
        assert_eq!(map.get("reviewer"), Some(&"reviewer-uuid".to_owned()));
    }

    /// Workers from OTHER projects must NOT appear in this project's
    /// resume map - their storage key doesn't equal this project's
    /// run-dir key.
    #[test]
    fn build_resume_map_filters_out_workers_from_other_projects() {
        let project_dir = std::path::Path::new("/Users/me/Projects/forge");
        let sessions = vec![
            mk_info(
                "ours",
                Some("/Users/me/Projects/forge/.claude/worktrees/planner"),
                Some("forge:worker:planner"),
            ),
            mk_info(
                "theirs",
                Some("/Users/me/Projects/granite/.claude/worktrees/planner"),
                Some("forge:worker:planner"),
            ),
        ];
        let map = build_resume_map_from_sessions(&sessions, project_dir, true);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("planner"), Some(&"ours".to_owned()));
    }

    /// Exact run-dir matching keeps a project at
    /// `/Users/me/Projects/forge` from picking up workers of a sibling
    /// project at `/Users/me/Projects/forge-old`: the sibling's worktree
    /// storage key never equals this project's `worker_tag_dir` run-dir
    /// key, so `forge-old`'s workers can't migrate into `forge`'s resume
    /// map even though the two paths share a byte-prefix.
    #[test]
    fn build_resume_map_filters_out_workers_with_overlapping_path_prefix() {
        let project_dir = std::path::Path::new("/Users/me/Projects/forge");
        let sessions = vec![
            mk_info(
                "ours",
                Some("/Users/me/Projects/forge/.claude/worktrees/planner"),
                Some("forge:worker:planner"),
            ),
            mk_info(
                "prefix-overlap",
                Some("/Users/me/Projects/forge-old/.claude/worktrees/planner"),
                Some("forge:worker:planner"),
            ),
        ];
        let map = build_resume_map_from_sessions(&sessions, project_dir, true);
        assert_eq!(map.len(), 1, "only the matching-project worker should resume");
        assert_eq!(map.get("planner"), Some(&"ours".to_owned()));
    }

    /// Across sessions merged from multiple account config_dirs the
    /// newest session per label wins, regardless of concat order.
    #[test]
    fn build_resume_map_keeps_newest_session_per_label() {
        let project_dir = std::path::Path::new("/Users/me/Projects/forge");
        let worktree = "/Users/me/Projects/forge/.claude/worktrees/planner";
        let mut older = mk_info("old-uuid", Some(worktree), Some("forge:worker:planner"));
        older.last_modified = 100;
        let mut newer = mk_info("new-uuid", Some(worktree), Some("forge:worker:planner"));
        newer.last_modified = 200;
        // Older listed first, as a cross-account concat can produce.
        let map = build_resume_map_from_sessions(&[older, newer], project_dir, true);
        assert_eq!(map.get("planner"), Some(&"new-uuid".to_owned()));
    }

    /// Lead-tagged sessions and untagged sessions are filtered out -
    /// only `forge:worker:*`-tagged sessions count.
    #[test]
    fn build_resume_map_ignores_non_worker_tags() {
        let project_dir = std::path::Path::new("/Users/me/Projects/forge");
        let sessions = vec![
            mk_info(
                "lead",
                Some("/Users/me/Projects/forge"),
                Some(forge_primitives::FORGE_LEAD_TAG),
            ),
            mk_info("legacy-untagged", Some("/Users/me/Projects/forge"), None),
            mk_info(
                "planner",
                Some("/Users/me/Projects/forge/.claude/worktrees/planner"),
                Some("forge:worker:planner"),
            ),
        ];
        let map = build_resume_map_from_sessions(&sessions, project_dir, true);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("planner"));
    }

    /// Non-git project: workers run in the project's main cwd (no
    /// worktree), so `worker_tag_dir` leaves the run dir at
    /// `project_dir`. The project-root-key session is picked; a stray
    /// (newer) worktree-key session is excluded because its storage
    /// folder is not the non-git worker's run dir.
    #[test]
    fn build_resume_map_finds_workers_in_non_git_project() {
        let project_dir = std::path::Path::new("/Users/me/Projects/non-git");
        let root =
            mk_info("tester-uuid", Some("/Users/me/Projects/non-git"), Some("forge:worker:tester"));
        let mut stray_worktree = mk_info(
            "stray-worktree",
            Some("/Users/me/Projects/non-git/.claude/worktrees/tester"),
            Some("forge:worker:tester"),
        );
        stray_worktree.last_modified = 999;
        let map = build_resume_map_from_sessions(&[root, stray_worktree], project_dir, false);
        assert_eq!(
            map.get("tester"),
            Some(&"tester-uuid".to_owned()),
            "a non-git worker resumes its project-root session, never a stray worktree-key one",
        );
    }

    /// A worker session with no resolvable storage key (empty - a scan
    /// that couldn't read the parent dir name, or a write race) is
    /// skipped rather than panicking.
    #[test]
    fn build_resume_map_skips_session_with_empty_storage_key() {
        let project_dir = std::path::Path::new("/Users/me/Projects/forge");
        let sessions = vec![mk_info("orphan", None, Some("forge:worker:planner"))];
        let map = build_resume_map_from_sessions(&sessions, project_dir, true);
        assert!(map.is_empty());
    }

    /// Duplicate label sightings: first hit wins. With
    /// `list_sessions` returning sorted-by-mtime-desc, "first" is the
    /// most recent worker JSONL for that label, which is the right
    /// resume target.
    #[test]
    fn build_resume_map_first_hit_wins_for_duplicate_labels() {
        let project_dir = std::path::Path::new("/Users/me/Projects/forge");
        let sessions = vec![
            mk_info(
                "newer",
                Some("/Users/me/Projects/forge/.claude/worktrees/planner"),
                Some("forge:worker:planner"),
            ),
            mk_info(
                "older",
                Some("/Users/me/Projects/forge/.claude/worktrees/planner"),
                Some("forge:worker:planner"),
            ),
        ];
        let map = build_resume_map_from_sessions(&sessions, project_dir, true);
        assert_eq!(map.get("planner"), Some(&"newer".to_owned()));
    }

    /// A git worker whose NEWEST tagged session lives at the repo root
    /// (an accumulated legacy session) but which also has an older
    /// worktree session must resume the WORKTREE session: that is the
    /// only one the worktree resume read path can find. Picking the
    /// newer repo-root session diverges from the read path and yields
    /// an empty resume.
    #[test]
    fn build_resume_map_scopes_git_worker_to_worktree_dir() {
        let project_dir = std::path::Path::new("/Users/me/Projects/playground");
        let mut root_newer = mk_info(
            "root-newer",
            Some("/Users/me/Projects/playground"),
            Some("forge:worker:gpt-tutor"),
        );
        root_newer.last_modified = 200;
        let mut wt_older = mk_info(
            "wt-older",
            Some("/Users/me/Projects/playground/.claude/worktrees/gpt-tutor"),
            Some("forge:worker:gpt-tutor"),
        );
        wt_older.last_modified = 100;
        let map = build_resume_map_from_sessions(&[root_newer, wt_older], project_dir, true);
        assert_eq!(map.get("gpt-tutor"), Some(&"wt-older".to_owned()));
    }

    /// Non-regression guard for the working durable workers
    /// (steward / credit-supply / wgpu-pr): the fix must keep them
    /// resuming their worktree session. steward carries a stray repo-root
    /// session that is NEWER than its worktree session - the exact
    /// gpt-tutor shape, where the removed "newest under project" logic
    /// would pick the root session and break the resume. Exact run-dir
    /// matching still resumes steward's worktree session. credit-supply
    /// and wgpu-pr have only their worktree session.
    #[test]
    fn build_resume_map_working_workers_pick_worktree_over_newer_root() {
        let project_dir = std::path::Path::new("/Users/me/Projects/granite");
        let wt = |label: &str| format!("/Users/me/Projects/granite/.claude/worktrees/{label}");
        let mut steward_wt =
            mk_info("steward-wt", Some(&wt("steward")), Some("forge:worker:steward"));
        steward_wt.last_modified = 300;
        let mut steward_root = mk_info(
            "steward-root",
            Some("/Users/me/Projects/granite"),
            Some("forge:worker:steward"),
        );
        steward_root.last_modified = 400;
        let credit_wt =
            mk_info("credit-wt", Some(&wt("credit-supply")), Some("forge:worker:credit-supply"));
        let wgpu_wt = mk_info("wgpu-wt", Some(&wt("wgpu-pr")), Some("forge:worker:wgpu-pr"));
        let map = build_resume_map_from_sessions(
            &[steward_wt, steward_root, credit_wt, wgpu_wt],
            project_dir,
            true,
        );
        assert_eq!(map.get("steward"), Some(&"steward-wt".to_owned()));
        assert_eq!(map.get("credit-supply"), Some(&"credit-wt".to_owned()));
        assert_eq!(map.get("wgpu-pr"), Some(&"wgpu-wt".to_owned()));
    }

    /// A worker's recorded `cwd` comes from the lite metadata read of
    /// the transcript head, so a `cwd` row past that window reports a
    /// wrong/fallback value. Scoping by the storage folder the session
    /// physically lives in resumes it regardless of the head-read cwd.
    #[test]
    fn build_resume_map_picks_worktree_session_by_storage_key_despite_wrong_cwd() {
        let project_dir = std::path::Path::new("/Users/me/Projects/playground");
        let worktree = "/Users/me/Projects/playground/.claude/worktrees/gpt-tutor";
        let mut info = mk_info("wt-uuid", Some(worktree), Some("forge:worker:gpt-tutor"));
        // Storage key stays the worktree dir (where the file lives); the
        // recorded cwd is a wrong value, as a head-read miss produces.
        info.cwd = Some("/Users/me/Projects/playground".to_owned());
        let map = build_resume_map_from_sessions(&[info], project_dir, true);
        assert_eq!(
            map.get("gpt-tutor"),
            Some(&"wt-uuid".to_owned()),
            "storage-key scoping resumes the worktree session even when the head-read cwd is wrong",
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod worker_resume_kick_skip_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_jsonl(config_dir: &std::path::Path, session_id: &str, body: &str) {
        let project_dir = forge_sdk::projects_dir_for(config_dir).join("test-proj");
        fs::create_dir_all(&project_dir).expect("project dir");
        let path = project_dir.join(format!("{session_id}.jsonl"));
        fs::write(&path, body).expect("write jsonl");
    }

    /// #157: 0 user turns (empty JSONL or no file) means the worker
    /// hasn't progressed past the kick. Re-kick on resume.
    #[test]
    fn worker_has_progress_past_kick_zero_user_turns_returns_false() {
        let cfg = tempdir().expect("cfg");
        let session_id = "550e8400-e29b-41d4-a716-446655440010";
        write_jsonl(cfg.path(), session_id, "");
        let (workspace, _) = Workspace::testing_stub_with_config_dir(cfg.path().to_path_buf());
        assert!(!workspace.worker_has_progress_past_kick(session_id));
    }

    /// #157: 1 user turn means the kick landed but the worker
    /// crashed / didn't progress. Re-fire so work actually starts.
    #[test]
    fn worker_has_progress_past_kick_one_user_turn_returns_false() {
        let cfg = tempdir().expect("cfg");
        let session_id = "550e8400-e29b-41d4-a716-446655440011";
        let body = r#"{"type":"user","timestamp":"2026-04-22T00:00:00.000Z","cwd":"/p","message":{"content":"kick"}}
"#;
        write_jsonl(cfg.path(), session_id, body);
        let (workspace, _) = Workspace::testing_stub_with_config_dir(cfg.path().to_path_buf());
        assert!(!workspace.worker_has_progress_past_kick(session_id));
    }

    /// #157: 2+ user turns means the worker is past the kick. Skip
    /// re-kicking to preserve in-flight state.
    #[test]
    fn worker_has_progress_past_kick_two_user_turns_returns_true() {
        let cfg = tempdir().expect("cfg");
        let session_id = "550e8400-e29b-41d4-a716-446655440012";
        let body = r#"{"type":"user","timestamp":"2026-04-22T00:00:00.000Z","cwd":"/p","message":{"content":"kick"}}
{"type":"user","timestamp":"2026-04-22T00:01:00.000Z","cwd":"/p","message":{"content":"follow-up"}}
"#;
        write_jsonl(cfg.path(), session_id, body);
        let (workspace, _) = Workspace::testing_stub_with_config_dir(cfg.path().to_path_buf());
        assert!(workspace.worker_has_progress_past_kick(session_id));
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod async_worker_spawn_failure_tests {
    use super::*;
    use crate::mcp::workers::types::WorkerEntry;

    fn fake_worker(label: &str, synth_key: &str, lead_id: &str, is_git: bool) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: "test".to_owned(),
            session_key: SessionKey::from_session_id(synth_key),
            status: forge_primitives::WorkerLiveness::Spawning,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: lead_id.to_owned(),
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
    fn install_lead_in_pool(workspace: &Arc<Workspace>, lead_id: &str) -> SessionKey {
        let key = SessionKey::from_session_id(lead_id);
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        workspace.pool.lock().insert(
            key.clone(),
            PooledAgent { handle: Arc::new(handle), account: AccountKey("test".to_owned()) },
        );
        key
    }

    /// Pins the lookup predicate the Connected catalog-mirror guard (in
    /// session_task) depends on: worker_lookup_for_session keyed by the
    /// pre-rekey synth key resolves a seeded worker (so its tag-less
    /// mirror is skipped) and not a lead/regular session. The guard's
    /// end-to-end skip is exercised in production.
    #[test]
    fn worker_lookup_drives_catalog_mirror_skip() {
        let (workspace, _rx) = Workspace::testing_stub();
        let project_key = ProjectKey::new("proj-x");
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        workspace.insert_live_worker(
            &project_key,
            fake_worker("reviewer", synth_key, "lead-uuid", false),
        );
        assert!(
            workspace.worker_lookup_for_session(&SessionKey::from_session_id(synth_key)).is_some(),
            "seeded worker must be detected so its tag-less catalog mirror is skipped"
        );
        assert!(
            workspace
                .worker_lookup_for_session(&SessionKey::from_session_id("lead-uuid"))
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
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        let lead_id = "lead-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", synth_key, lead_id, true));
        let lead_key = install_lead_in_pool(&workspace, lead_id);

        let handled = workspace.handle_async_worker_spawn_failure(
            &SessionKey::from_session_id(synth_key),
            "fatal: 'reviewer' is already used by worktree at /a/b/c",
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
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        let lead_id = "lead-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", synth_key, lead_id, true));
        install_lead_in_pool(&workspace, lead_id);

        let handled = workspace.handle_async_worker_spawn_failure(
            &SessionKey::from_session_id(synth_key),
            "agent spawn failed: subprocess exited with code 2",
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
        let unknown = SessionKey::from_session_id("not-a-worker");
        let handled = workspace.handle_async_worker_spawn_failure(&unknown, "some unrelated error");
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
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        let lead_id = "lead-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", synth_key, lead_id, true));
        install_lead_in_pool(&workspace, lead_id);

        let session_key = SessionKey::from_session_id(synth_key);
        let worktree_msg = "fatal: 'reviewer' is already used by worktree at /a";
        // Pin the test's premise: this message must classify as
        // WorktreeCreationFailed so we exercise the remove-then-no-op
        // path. If a future change to the classifier breaks this
        // assumption, the test below would change shape (transition-
        // to-Failed isn't a no-op on re-fire).
        assert!(
            matches!(
                crate::mcp::workers::facade::classify_worker_spawn_failure(worktree_msg, true),
                crate::mcp::workers::facade::WorkerSpawnError::WorktreeCreationFailed { .. },
            ),
            "test fixture must classify as worktree failure to exercise the removal path",
        );

        assert!(workspace.handle_async_worker_spawn_failure(&session_key, worktree_msg));
        let _ = workspace.drain_test_dispatch_buffer();

        // Second call: WorkerEntry already gone, returns false, no
        // new dispatch.
        assert!(!workspace.handle_async_worker_spawn_failure(&session_key, worktree_msg));
        assert!(workspace.drain_test_dispatch_buffer().is_empty());
    }

    fn install_db(workspace: &Arc<Workspace>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        workspace
            .install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("db"));
        dir
    }

    fn persisted_labels(workspace: &Arc<Workspace>, project: &str) -> Vec<String> {
        let guard = workspace.db.lock();
        crate::store::dynamic_workers::list_for_project(guard.as_ref().expect("db"), project)
            .expect("list")
            .into_iter()
            .map(|w| w.label)
            .collect()
    }

    /// #2: a worktree-creation failure is a hard removal (the worker
    /// never started), so it deletes the persisted dynamic-worker row -
    /// otherwise the row zombie-re-spawns every restart despite a
    /// visibly-failed spawn.
    #[tokio::test]
    async fn worktree_failure_deletes_persisted_dynamic_worker_row() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let _dir = install_db(&workspace);
        workspace.enable_test_dispatch_intercept();

        let project_key = ProjectKey::new("proj-x");
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        let lead_id = "lead-uuid";
        let _ = workspace.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: "proj-x".to_owned(),
            label: "reviewer".to_owned(),
            charter: "c".to_owned(),
            kick: None,
        });
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", synth_key, lead_id, true));
        install_lead_in_pool(&workspace, lead_id);

        let session_key = SessionKey::from_session_id(synth_key);
        let worktree_msg = "fatal: 'reviewer' is already used by worktree at /a";
        assert!(workspace.handle_async_worker_spawn_failure(&session_key, worktree_msg));

        assert!(
            persisted_labels(&workspace, "proj-x").is_empty(),
            "worktree-failure hard removal deletes the persisted row",
        );
    }

    /// #2: a non-worktree failure transitions the worker to Failed
    /// (visible) and KEEPS its row, so it re-spawns on the next restart
    /// to recover or re-fail visibly.
    #[tokio::test]
    async fn transition_to_failed_keeps_persisted_dynamic_worker_row() {
        let (workspace, _update_rx) = Workspace::testing_stub();
        let _dir = install_db(&workspace);

        let project_key = ProjectKey::new("proj-x");
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        let _ = workspace.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: "proj-x".to_owned(),
            label: "reviewer".to_owned(),
            charter: "c".to_owned(),
            kick: None,
        });
        // Non-git worker + a generic message classifies as DispatchFailed,
        // driving the transition-to-Failed (visible) path.
        workspace.insert_live_worker(
            &project_key,
            fake_worker("reviewer", synth_key, "lead-uuid", false),
        );

        let session_key = SessionKey::from_session_id(synth_key);
        assert!(
            workspace
                .handle_async_worker_spawn_failure(&session_key, "subprocess exited with code 2")
        );

        assert_eq!(
            persisted_labels(&workspace, "proj-x"),
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
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        let session_key = SessionKey::from_session_id(synth_key);
        // Worker starts Spawning + needs_tag = true (mirrors
        // fresh-spawn state pre-Connected).
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", synth_key, "lead", true));

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
        let synth_key = "__spawn_worker_proj-x_builder_abc__";
        let session_key = SessionKey::from_session_id(synth_key);
        workspace.insert_live_worker(&project_key, fake_worker("builder", synth_key, "lead", true));

        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: crate::mcp::peers::types::AskChannel::Workers,
                caller: SessionKey::from_str_for_test("lead-1"),
                target_project: crate::mcp::workers::worker_target_project_key("proj-x", "builder"),
                target_session: None,
            },
        );

        workspace.handle_async_worker_spawn_failure(&session_key, "resume failed: boom");
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
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        let session_key = SessionKey::from_session_id(synth_key);
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", synth_key, "lead", true));

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
        let synth_key = "__spawn_worker_proj-x_reviewer_abc__";
        let lead_id = "lead-gone-uuid";
        workspace
            .insert_live_worker(&project_key, fake_worker("reviewer", synth_key, lead_id, true));
        // DELIBERATELY skip install_lead_in_pool - lead is "gone".

        let handled = workspace.handle_async_worker_spawn_failure(
            &SessionKey::from_session_id(synth_key),
            "fatal: 'reviewer' is already used by worktree at /a",
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

    fn worker_entry(label: &str, session_key: &SessionKey, is_git: bool) -> WorkerEntry {
        WorkerEntry {
            label: label.into(),
            charter: "test charter".into(),
            session_key: session_key.clone(),
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".into(),
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
    ) -> (std::path::PathBuf, SessionKey) {
        ws.seed_test_project_with_static_workers(project_name, project_root, &[]);
        let project_key = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some(project_root)),
        );
        let session_key = SessionKey::from_session_id(worker_session);
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
        ws.seed_test_project_with_static_workers("forge", "/tmp/test-forge-lead", &[]);
        let lead_key = SessionKey::from_session_id("lead-uuid");
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
    // #245 Layer B: resume_cwd_for_session falls back to the owning
    // worker's project_root when the catalog has no recorded cwd.
    // Without this, claude --resume inherits the forge binary's
    // process cwd and derives the JSONL location against the wrong
    // git root (the bug documented in #245).
    // ---------------------------------------------------------------

    #[test]
    fn resume_cwd_for_session_returns_worktree_for_git_worker_with_no_catalog_cwd() {
        // Git-repo worker (the hub-modules babysitter / librarian
        // case from #245). Layer B composes the worker's worktree
        // path so claude resolves the JSONL on the first try -
        // passing just the project root would make claude look under
        // the wrong sanitised dir and surface "No conversation
        // found".
        let (ws, _rx) = Workspace::testing_stub();
        let (project_root, session_key) = seed_project_and_worker(
            &ws,
            "hub-modules",
            "/tmp/test-hub-modules",
            "babysitter",
            "worker-uuid-hub",
            true,
        );
        let resolved = ws.resume_cwd_for_session(&session_key);
        assert_eq!(
            resolved,
            project_root.join(".claude/worktrees/babysitter").to_string_lossy(),
            "git-repo worker resume cwd must compose the worktree path via worker_tag_dir",
        );
    }

    #[test]
    fn resume_cwd_for_session_returns_project_root_for_non_git_worker() {
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
        let resolved = ws.resume_cwd_for_session(&session_key);
        assert_eq!(
            resolved,
            project_root.to_string_lossy(),
            "non-git worker resume cwd must equal the project root (no worktree subdir)",
        );
    }

    #[test]
    fn resume_cwd_for_session_returns_empty_for_unknown_session() {
        // Non-worker, non-catalog session - the function returns
        // empty string and lets the bridge surface ConnectionFailed
        // (current behaviour for genuinely-orphan sessions).
        let (ws, _rx) = Workspace::testing_stub();
        let unknown = SessionKey::from_session_id("not-a-known-session");
        assert_eq!(ws.resume_cwd_for_session(&unknown), "");
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
            "granite-backend",
            "/tmp/test-granite-cwd",
            "pyth-review-fixes",
            "worker-uuid-cwd",
            true,
        );
        assert_eq!(
            ws.cwd_for_session(&session_key).as_deref(),
            project_root.join(".claude/worktrees/pyth-review-fixes").to_str(),
        );
    }

    /// A worker has no catalog row, so `project_accounts_for`'s
    /// `session_cwd_for` resolution misses it and falls back to the
    /// default project. Env must not inherit that fallback: a worker
    /// resolves to the project whose worktree it runs in.
    #[test]
    fn project_for_target_resolves_a_worker_session_to_its_project() {
        let (ws, _rx) = Workspace::testing_stub();
        let (_root, session_key) = seed_project_and_worker(
            &ws,
            "granite-backend",
            "/tmp/test-granite-env",
            "pyth-review-fixes",
            "worker-uuid-env",
            true,
        );
        assert!(
            ws.session_cwd_for(&session_key).is_none(),
            "precondition: the catalog holds no worker rows",
        );
        assert_eq!(
            ws.project_for_target(&SessionTarget::Session(session_key)).map(|p| p.name).as_deref(),
            Some("granite-backend"),
        );
    }

    /// An unresolvable target must keep the account env rather than
    /// borrowing whichever project happens to be the default - that
    /// would hand one project's declared keys to another's session.
    #[test]
    fn session_env_for_unresolved_target_keeps_the_account_env() {
        let (ws, _rx) = Workspace::testing_stub();
        let account_env: std::collections::HashMap<String, String> =
            [("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned())]
                .into_iter()
                .collect();
        let orphan = SessionTarget::Session(SessionKey::from_session_id("not-a-known-session"));
        assert!(
            ws.project_for_target(&orphan).is_none(),
            "precondition: target resolves to no project"
        );
        assert_eq!(
            ws.session_env_for(&orphan, &account_env),
            account_env,
            "unresolved target -> account env verbatim",
        );
    }

    #[test]
    fn cwd_for_session_is_none_when_the_workers_project_is_not_loaded() {
        // The contradiction the WARN names: the registry knows the
        // worker's project_key and label, but no loaded project
        // matches that key, so no path can be composed. Unreachable
        // while forge.toml and `live_workers` agree.
        let (ws, _rx) = Workspace::testing_stub();
        let session_key = SessionKey::from_session_id("worker-uuid-orphan");
        ws.insert_live_worker(
            &ProjectKey::new("stale-key".to_owned()),
            worker_entry("implementer", &session_key, true),
        );
        assert!(ws.cwd_for_session(&session_key).is_none());
    }

    #[test]
    fn resume_cwd_for_session_prefers_catalog_cwd_over_worker_fallback() {
        // When the catalog DOES carry a cwd for the session, the
        // catalog path wins - the worker fallback is a fallback, not
        // an override. Lead-resume behaviour stays unchanged: leads
        // are catalog-recorded, so they always hit branch 1.
        //
        // To prove the precedence we seed a worker entry AND a
        // catalog row for the same session_key, with DIFFERENT cwd
        // values. The catalog cwd must win.
        let (ws, _rx) = Workspace::testing_stub();
        let project_root = "/tmp/test-precedence";
        let session_id = "shared-session-uuid";
        let (_, session_key) = seed_project_and_worker(
            &ws,
            "precedence-proj",
            project_root,
            "implementer",
            session_id,
            true,
        );
        // Seed a catalog cwd for the same session_id pointing at a
        // distinct path; the precedence test fails if the worker
        // fallback overrides it.
        let catalog_cwd = "/tmp/test-precedence-catalog-cwd";
        ws.record_connected_session(catalog_cwd, session_id, None);
        let resolved = ws.resume_cwd_for_session(&session_key);
        assert_eq!(resolved, catalog_cwd, "catalog cwd must win over the worker_tag_dir fallback");
    }

    /// Read/pick consistency: the cwd `resume_cwd_for_session` hands
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
            &ws.resume_cwd_for_session(&session_key),
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
    // #246: recompute_plan_if_ready + extend_plan_for_adhoc_worker +
    // session_chip_for. Build a real workspace from the local
    // `make_workspace_dir_246` helper (single account "Subspace",
    // single project "forge") + manually drive the loading state via
    // account_states().lock().set_*().
    // ---------------------------------------------------------------

    fn make_workspace_dir_246() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// Two org accounts in definition order (Alpha, Beta) and a
    /// project with NO `accounts` allow-list, so the project pool IS
    /// the org-ordered ready slice - the exact shape where HashMap
    /// iteration order used to make the lead-account pick
    /// non-deterministic across restarts.
    fn make_workspace_dir_246_two_accounts() -> tempfile::TempDir {
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
config_dir = "~/.claude-alpha"

[[accounts]]
display_name = "Beta"
config_dir = "~/.claude-beta"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    /// An experimental account defined FIRST (Exp), a regular account
    /// second (Alpha), both pinned by the org, project with no
    /// allow-list. Without the experimental exclusion the lead would
    /// bind to definition-order pool[0] = Exp; the exclusion forces it
    /// onto Alpha.
    fn make_workspace_dir_experimental() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Exp", "Alpha"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Exp"
config_dir = "~/.claude-exp"
experimental = true

[[accounts]]
display_name = "Alpha"
config_dir = "~/.claude-alpha"
"#,
        )
        .expect("write forge.toml");
        dir
    }

    #[tokio::test]
    async fn recompute_plan_if_ready_binds_lead_in_account_definition_order() {
        // Two ready accounts + an empty project allow-list: the pool is
        // the org-ordered ready slice and the lone project's lead lands
        // at pool[0]. The lead must bind to the first definition-order
        // account (Alpha), not whatever HashMap iteration would surface
        // - reverting `ordered_keys` to `by_key` makes this flaky.
        let dir = make_workspace_dir_246_two_accounts();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            for name in ["Alpha", "Beta"] {
                let snapshot = forge_primitives::usage::UsageSnapshot {
                    source: forge_primitives::usage::UsageSourceKind::Oauth,
                    fetched_at: std::time::SystemTime::UNIX_EPOCH,
                    five_hour: None,
                    seven_day: None,
                    seven_day_opus: None,
                    seven_day_sonnet: None,
                    extra_usage: None,
                };
                accounts.set_usage(&AccountKey(name.to_owned()), snapshot);
            }
        }

        workspace.recompute_plan_if_ready();
        let plan = workspace.assignment_plan.lock();
        let plan = plan.as_ref().expect("plan populates once all_loaded fires");
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        assert_eq!(
            plan.lookup(&project_key, &"lead".to_owned()).cloned(),
            Some(AccountKey("Alpha".to_owned())),
            "lead must bind to the first definition-order account, not a HashMap-order pick",
        );
    }

    #[tokio::test]
    async fn recompute_plan_if_ready_excludes_experimental_from_pool() {
        // Exp is defined first and pinned by the org, but marked
        // experimental. The assignment-plan pool must skip it: the
        // lead binds to Alpha (the non-experimental account), never to
        // definition-order pool[0] = Exp.
        let dir = make_workspace_dir_experimental();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            for name in ["Exp", "Alpha"] {
                let snapshot = forge_primitives::usage::UsageSnapshot {
                    source: forge_primitives::usage::UsageSourceKind::Oauth,
                    fetched_at: std::time::SystemTime::UNIX_EPOCH,
                    five_hour: None,
                    seven_day: None,
                    seven_day_opus: None,
                    seven_day_sonnet: None,
                    extra_usage: None,
                };
                accounts.set_usage(&AccountKey(name.to_owned()), snapshot);
            }
        }

        workspace.recompute_plan_if_ready();
        let plan = workspace.assignment_plan.lock();
        let plan = plan.as_ref().expect("plan populates once all_loaded fires");
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        assert_eq!(
            plan.lookup(&project_key, &"lead".to_owned()).cloned(),
            Some(AccountKey("Alpha".to_owned())),
            "experimental Exp is excluded from the pool; the lead binds to Alpha",
        );
    }

    #[tokio::test]
    async fn resolve_account_for_switch_resolves_experimental_account() {
        // The /account manual switch must be able to resolve an
        // experimental account - it is the only way one ever gets used.
        // resolve_account_for_switch feeds forced_account, which bypasses
        // the picker/plan entirely.
        let dir = make_workspace_dir_experimental();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let resolved = workspace.resolve_account_for_switch("Exp");
        assert_eq!(
            resolved.map(|(key, _)| key),
            Some(AccountKey("Exp".to_owned())),
            "an experimental account still resolves for the /account switch",
        );
    }

    #[tokio::test]
    async fn recompute_plan_if_ready_noop_while_loading() {
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        // Fresh workspace: account starts in `Loading`. all_loaded
        // returns false; recompute must not populate the plan.
        workspace.recompute_plan_if_ready();
        let plan = workspace.assignment_plan.lock();
        assert!(plan.is_none(), "plan stays None while accounts are still Loading");
    }

    #[tokio::test]
    async fn recompute_plan_if_ready_populates_plan_when_all_ready() {
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        // Transition the lone account to Ready by injecting a snapshot.
        {
            let mut accounts = workspace.account_states().lock();
            let snapshot = forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                five_hour: None,
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
            };
            accounts.set_usage(&AccountKey("Subspace".to_owned()), snapshot);
        }

        workspace.recompute_plan_if_ready();
        let plan = workspace.assignment_plan.lock();
        let plan = plan.as_ref().expect("plan populates once all_loaded fires");
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        assert!(
            !plan.project_has_no_assignments(&project_key),
            "plan must have at least one assignment for the lone project",
        );
    }

    #[tokio::test]
    async fn recompute_plan_if_ready_uses_frozen_overlay_on_recompute() {
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            let snapshot = forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                five_hour: None,
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
            };
            accounts.set_usage(&AccountKey("Subspace".to_owned()), snapshot);
        }

        workspace.recompute_plan_if_ready();
        let first_plan = workspace.assignment_plan.lock().clone();

        // Recompute should be idempotent on the same ready set
        // (frozen overlay merges; existing assignments preserved).
        workspace.recompute_plan_if_ready();
        let second_plan = workspace.assignment_plan.lock().clone();
        assert!(first_plan.is_some());
        assert!(second_plan.is_some());
        // Same plan contents (the frozen overlay preserves entries).
        let first = first_plan.expect("first");
        let second = second_plan.expect("second");
        assert_eq!(
            first.lookup(
                &ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
                    Some(workspace.config.projects[0].path.to_string_lossy().as_ref())
                ),),
                &"lead".to_owned(),
            ),
            second.lookup(
                &ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
                    Some(workspace.config.projects[0].path.to_string_lossy().as_ref())
                ),),
                &"lead".to_owned(),
            ),
            "frozen overlay preserves the same lead assignment across recomputes",
        );
    }

    #[tokio::test]
    async fn extend_plan_for_adhoc_worker_noop_when_plan_unpopulated() {
        // Before the plan is populated, the helper must be a no-op
        // (doesn't panic; doesn't side-effect through to lookups).
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        workspace.extend_plan_for_adhoc_worker(&project_key, "reviewer");
        assert!(workspace.assignment_plan.lock().is_none(), "plan still unpopulated");
    }

    #[tokio::test]
    async fn extend_plan_for_adhoc_worker_extends_when_plan_populated() {
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            let snapshot = forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                five_hour: None,
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
            };
            accounts.set_usage(&AccountKey("Subspace".to_owned()), snapshot);
        }
        workspace.recompute_plan_if_ready();
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let assigned = workspace.extend_plan_for_adhoc_worker(&project_key, "reviewer");
        assert!(
            assigned.is_none(),
            "assigning to a usable account returns None (nothing to surface)",
        );
        let plan = workspace.assignment_plan.lock();
        let plan = plan.as_ref().expect("populated");
        assert!(
            plan.lookup(&project_key, &"reviewer".to_owned()).is_some(),
            "extend_plan_for_adhoc_worker adds the adhoc label to the plan",
        );
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
        }
    }

    #[tokio::test]
    async fn extend_plan_for_adhoc_worker_returns_saturated_account_when_pool_all_saturated() {
        // The lone allowed account is at 100% utilization, so the plan
        // pool falls back to it (nothing else exists) and the adhoc
        // rotation has no usable slot to move to. extend_plan returns
        // the saturated account it landed on so the spawn path can
        // surface the rate-limited state.
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        workspace
            .account_states()
            .lock()
            .set_usage(&AccountKey("Subspace".to_owned()), usage_at(100.0));
        workspace.recompute_plan_if_ready();
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let assigned = workspace.extend_plan_for_adhoc_worker(&project_key, "reviewer");
        assert_eq!(
            assigned,
            Some(AccountKey("Subspace".to_owned())),
            "all-saturated pool surfaces the assigned rate-limited account",
        );
    }

    #[tokio::test]
    async fn extend_plan_for_adhoc_worker_rotates_onto_usable_account() {
        // Both accounts are usable at compute time, so the plan freezes
        // the pool as [Alpha, Beta] and the first adhoc slot resolves to
        // Beta. Beta then hits its cap AFTER the freeze. The rotation
        // must walk off the now-saturated slot onto the still-usable
        // Alpha and return None (a usable assignment, nothing to surface).
        let dir = make_workspace_dir_246_two_accounts();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            accounts.set_usage(&AccountKey("Alpha".to_owned()), usage_at(10.0));
            accounts.set_usage(&AccountKey("Beta".to_owned()), usage_at(10.0));
        }
        workspace.recompute_plan_if_ready();
        // Beta saturates after the pool was frozen.
        workspace
            .account_states()
            .lock()
            .set_usage(&AccountKey("Beta".to_owned()), usage_at(100.0));
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let assigned = workspace.extend_plan_for_adhoc_worker(&project_key, "reviewer");
        assert!(assigned.is_none(), "rotating onto a usable account returns None");
        let plan = workspace.assignment_plan.lock();
        let plan = plan.as_ref().expect("populated");
        assert_eq!(
            plan.lookup(&project_key, &"reviewer".to_owned()),
            Some(&AccountKey("Alpha".to_owned())),
            "adhoc worker rotates off the saturated Beta onto the usable Alpha",
        );
    }

    #[tokio::test]
    async fn extend_plan_for_adhoc_worker_preserves_pinned_account_gone_unusable() {
        // A re-spawn under an existing label keeps its original account
        // (wire identity) via the idempotent early-return, even after
        // that account goes rate-limited while others stay usable. The
        // re-check must NOT re-home the pin, but extend_plan still
        // surfaces the now-unusable state by returning the account.
        let dir = make_workspace_dir_246_two_accounts();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            accounts.set_usage(&AccountKey("Alpha".to_owned()), usage_at(10.0));
            accounts.set_usage(&AccountKey("Beta".to_owned()), usage_at(10.0));
        }
        workspace.recompute_plan_if_ready();
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        // First spawn pins "reviewer" to the usable Beta (adhoc slot 1).
        let first = workspace.extend_plan_for_adhoc_worker(&project_key, "reviewer");
        assert!(first.is_none(), "initial pin onto a usable account returns None");
        {
            let plan = workspace.assignment_plan.lock();
            let plan = plan.as_ref().expect("populated");
            assert_eq!(
                plan.lookup(&project_key, &"reviewer".to_owned()),
                Some(&AccountKey("Beta".to_owned())),
            );
        }

        // Beta goes rate-limited; Alpha is still usable.
        workspace
            .account_states()
            .lock()
            .set_usage(&AccountKey("Beta".to_owned()), usage_at(100.0));

        // Re-spawn the same label: the pin is preserved (not re-homed
        // onto Alpha) and the now-unusable state is surfaced.
        let second = workspace.extend_plan_for_adhoc_worker(&project_key, "reviewer");
        assert_eq!(
            second,
            Some(AccountKey("Beta".to_owned())),
            "re-spawn keeps the pinned account and surfaces its unusable state",
        );
        let plan = workspace.assignment_plan.lock();
        let plan = plan.as_ref().expect("populated");
        assert_eq!(
            plan.lookup(&project_key, &"reviewer".to_owned()),
            Some(&AccountKey("Beta".to_owned())),
            "pinned account is never re-homed to a usable one",
        );
    }

    #[tokio::test]
    async fn session_chip_for_returns_none_when_plan_unpopulated() {
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        assert!(workspace.session_chip_for(&project_key, "lead").is_none());
    }

    #[tokio::test]
    async fn session_chip_for_normal_branch_for_ready_account() {
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            let snapshot = forge_primitives::usage::UsageSnapshot {
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
            };
            accounts.set_usage(&AccountKey("Subspace".to_owned()), snapshot);
        }
        workspace.recompute_plan_if_ready();
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key, "lead").expect("chip");
        assert_eq!(chip.state, SessionChipState::Normal);
        assert_eq!(chip.account_name, "Subspace");
    }

    #[tokio::test]
    async fn session_chip_for_at_cap_branch() {
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            let snapshot = forge_primitives::usage::UsageSnapshot {
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
            };
            accounts.set_usage(&AccountKey("Subspace".to_owned()), snapshot);
        }
        workspace.recompute_plan_if_ready();
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key, "lead").expect("chip");
        assert_eq!(chip.state, SessionChipState::AtCap);
    }

    #[tokio::test]
    async fn session_chip_for_at_cap_on_weekly_window() {
        // A weekly (7-day) cap alone, 5h window clear, must still flag
        // the chip AtCap - saturation is any-window, not 5h-only.
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            let snapshot = forge_primitives::usage::UsageSnapshot {
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
            };
            accounts.set_usage(&AccountKey("Subspace".to_owned()), snapshot);
        }
        workspace.recompute_plan_if_ready();
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key, "lead").expect("chip");
        assert_eq!(chip.state, SessionChipState::AtCap);
    }

    #[tokio::test]
    async fn session_chip_for_bailed_branch() {
        let dir = make_workspace_dir_246();
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        // Plan needs SOMETHING in it for session_chip_for to look up.
        // Snapshot first then transition to Bailed (preserves plan
        // assignment, just changes loading state).
        {
            let mut accounts = workspace.account_states().lock();
            let snapshot = forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                five_hour: None,
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
            };
            accounts.set_usage(&AccountKey("Subspace".to_owned()), snapshot);
        }
        workspace.recompute_plan_if_ready();
        // Now flip to Bailed.
        workspace
            .account_states()
            .lock()
            .set_loading(&AccountKey("Subspace".to_owned()), crate::account::LoadingState::Bailed);
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        let chip = workspace.session_chip_for(&project_key, "lead").expect("chip");
        assert_eq!(chip.state, SessionChipState::Bailed);
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

    /// Helper: synthetic session_key for kick tests.
    fn sk(name: &str) -> SessionKey {
        SessionKey::from_session_id(format!("kick-test-{name}"))
    }

    /// Helper: assert the intercept buffer's Prompt commands match
    /// `expected` SessionKeys in order. Filters out any non-Prompt
    /// commands the dispatch path might queue.
    fn assert_dispatched_kick_keys(
        workspace: &Arc<Workspace>,
        expected: &[SessionKey],
        context: &str,
    ) {
        let dispatched = workspace.drain_test_dispatch_buffer();
        let keys: Vec<SessionKey> = dispatched
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
        workspace
            .enqueue_kick(KickRequest { session_key: a.clone(), prompt_body: "kick a".into() });
        workspace
            .enqueue_kick(KickRequest { session_key: b.clone(), prompt_body: "kick b".into() });

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

        let keys: Vec<SessionKey> = (0..7).map(|i| sk(&format!("worker-{i}"))).collect();
        for key in &keys {
            workspace.enqueue_kick(KickRequest {
                session_key: key.clone(),
                prompt_body: format!("kick {}", key.as_str()),
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

        workspace.enqueue_kick(KickRequest { session_key: sk("only"), prompt_body: "k".into() });
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
        workspace.enqueue_kick(KickRequest { session_key: sk("orphan"), prompt_body: "k".into() });
        // No assertion target other than "we got here without panicking".
        // A future change that makes enqueue_kick require a started
        // dispatcher would fail this test.
        let _ = Duration::from_millis(0); // touch Duration to keep the use site live
    }
}
