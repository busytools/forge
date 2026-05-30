//! The orchestrator.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use forge_agent::AgentHandle;
use forge_agent::client::SessionLaunchSettings;
use forge_primitives::{PeerInflightStats, SDKSessionInfo};

use crate::mcp::peers::types::{CorrelationId, InflightAsk, WrappedKind, WrappedPrompt};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::account::{self, AccountKey, AccountStateMap};
use crate::config::{LoadedConfig, LoadedProject, load_from_dir};
use crate::domain_session::DomainSession;
use crate::error::WorkspaceError;
use crate::protocol::{Command, DispatchError, PendingInteractionSlot, SessionUpdate};
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

/// One enqueue onto the workspace-level worker-kick channel
/// (#259). Built by `maybe_kick_worker_on_connected` (and any
/// future kick site); drained by the workspace's
/// `start_kick_dispatcher` task, which fires one `Command::Prompt`
/// per `KICK_DISPATCH_INTERVAL` tick. Same payload shape as the
/// existing `Command::Prompt` carries (no attachments are ever
/// part of a kick prompt - kicks are pure text from
/// `<label>/kick.md` or `<label>/resume-kick.md`).
#[derive(Debug, Clone)]
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
    /// Account is Ready but its 5h budget window is currently
    /// capped. Yellow foreground signals "still works but expect
    /// throttle until the 5h window resets."
    FiveHourCap,
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
    /// [`Self::store_pending_interaction`] writes under the same lock
    /// the `SessionTask` actor uses to read+remove. `pub(crate)` so
    /// `mcp::peers::facade::WorkspaceFacade` can read
    /// `current_inbound_hop` without an extra wrapper method.
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
    /// Set the first time [`Self::start_usage_poller`] runs. Subsequent
    /// calls early-return to avoid spawning duplicate poller tasks.
    usage_poller_started: std::sync::atomic::AtomicBool,
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
    /// `seed_test_project_with_team` are searched first in
    /// `find_project_view_by_name` so tests can drive the
    /// Connected-hook team-spawn trigger without writing a
    /// real `forge.toml`. Empty in production.
    #[cfg(any(test, feature = "testing"))]
    test_extra_projects: Mutex<Vec<LoadedProject>>,
}

/// Pool entry wrapping the live `Arc<AgentHandle>`. Tests assert
/// which account each spawn was bound to; that binding lives behind
/// `cfg(test)` so production carries no dead field.
#[derive(Clone)]
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
#[must_use]
pub fn resolve_lead_session(sessions: &[SDKSessionInfo]) -> Option<&SDKSessionInfo> {
    let latest_with = |pred: fn(&&SDKSessionInfo) -> bool| -> Option<&SDKSessionInfo> {
        sessions.iter().filter(pred).max_by_key(|s| s.last_modified)
    };
    latest_with(|s| s.tag.as_deref() == Some(forge_primitives::FORGE_LEAD_TAG))
        .or_else(|| latest_with(|s| s.tag.is_none()))
}

/// Scan the catalog for `forge:worker:<label>` tagged sessions whose
/// `cwd` falls under `project_dir` (the project's filesystem root).
/// Returns one entry per role label, keyed by label and valued by
/// session_id. Used by the engineering-team Connected hook to decide
/// which roles to resume vs spawn fresh on forge restart.
///
/// Why scan the whole catalog rather than just `project_dir`'s own
/// subdir: workers spawned with `--worktree=<label>` `chdir` into
/// `<project_dir>/.claude/worktrees/<label>/` and claude indexes
/// their JSONLs under a DIFFERENT `<config_dir>/projects/<subdir>/`
/// keyed by the worktree path, not the main repo. A `directory=Some`
/// scan only walks one subdir; a worker in a worktree lives in a
/// SIBLING subdir, missing the filter. Switch to `directory=None`
/// (walk every project subdir) and filter by `cwd.starts_with
/// (project_dir)` so we catch:
///
/// - lead sessions: cwd == project_dir (matches)
/// - workers in non-git projects: cwd == project_dir (matches)
/// - workers in git worktrees: cwd == `<project_dir>/.claude/worktrees/<label>/`
///   (starts with project_dir, matches)
///
/// Workers from OTHER projects have cwds outside `project_dir` and
/// are filtered out. Untagged or `forge:lead`-tagged sessions are
/// filtered out by the tag-prefix check.
async fn scan_worker_resume_map(
    config_dir: &std::path::Path,
    project_dir: &std::path::Path,
) -> HashMap<String, String> {
    let sessions =
        forge_agent::userdata::catalog::scan::list_sessions(config_dir, None, None, 0, true).await;
    build_resume_map_from_sessions(&sessions, project_dir)
}

/// Pure-function inner of [`scan_worker_resume_map`] - pulls the
/// catalog scan out so the filtering logic can be unit-tested without
/// the async filesystem walk. Takes the already-scanned `sessions`
/// slice and a `project_dir` prefix; returns label -> session_id for
/// each worker-tagged session whose `cwd` is under `project_dir`.
///
/// Uses `Path::starts_with` (component-aware) rather than
/// `str::starts_with` so a project at `/foo/bar` doesn't match
/// workers from a sibling project at `/foo/bar-old` whose cwd
/// shares the byte-prefix.
#[must_use]
fn build_resume_map_from_sessions(
    sessions: &[SDKSessionInfo],
    project_dir: &std::path::Path,
) -> HashMap<String, String> {
    let mut resume_map: HashMap<String, String> = HashMap::new();
    for info in sessions {
        let Some(cwd) = info.cwd.as_deref().map(std::path::Path::new) else {
            continue;
        };
        if !cwd.starts_with(project_dir) {
            continue;
        }
        let Some(tag) = info.tag.as_deref() else {
            continue;
        };
        let Some(label) = tag.strip_prefix(forge_primitives::FORGE_WORKER_TAG_PREFIX) else {
            continue;
        };
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
        let config = load_from_dir(&config_dir)?;

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

        // Seed account usage from the on-disk forge-state.toml so
        // the launchpad picker has tier data immediately at cold
        // boot. Anthropic's /api/oauth/usage rate-limiter can stall
        // the first live probe for 30 s+; without seed data every
        // account ties at tier 0 (unknown-fresh) during that window.
        // The 60 s background poller refreshes these snapshots in
        // the background - the cache is purely "last known value"
        // seed.
        let state = crate::account_cache::load(&config_dir);
        accounts.seed_from_cache(&state.account_usage);

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
            usage_poller_started: std::sync::atomic::AtomicBool::new(false),
            kick_dispatcher_tx,
            kick_dispatcher_rx_slot: Mutex::new(Some(kick_dispatcher_rx)),
            proxy: Some(proxy),
            team_spawn_in_flight: Mutex::new(std::collections::HashSet::new()),
            #[cfg(any(test, feature = "testing"))]
            command_intercept: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            test_extra_projects: Mutex::new(Vec::new()),
        })
    }

    /// Handle to the workspace-owned wire-classification rewriter
    /// proxy, when one is bound. `None` only in `testing_stub` paths.
    pub fn proxy_handle(&self) -> Option<&forge_agent::proxy::ProxyHandle> {
        self.proxy.as_ref()
    }

    /// Return the names of all orgs in declaration order, paired
    /// with their pinned account list. The Projects pane uses this
    /// to drive the org-grouped tree render.
    pub fn list_orgs(&self) -> Vec<(String, Vec<String>)> {
        self.config.orgs.iter().map(|org| (org.name.clone(), org.accounts.clone())).collect()
    }

    /// Snapshot of the `[ui]` section from `forge.toml`. All fields
    /// have defaults so callers can use the result without worrying
    /// about whether the section was present in the config file.
    /// Cheap clone - the struct is shallow.
    pub fn ui_settings(&self) -> crate::ui::UiSettings {
        self.config.ui.clone()
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
        let mut views = Vec::with_capacity(self.config.projects.len());
        for project in &self.config.projects {
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
                team: project.team.clone(),
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
        self.get_agent_handle_with_spawn_key(target, settings, None)
    }

    /// Like [`Self::get_agent_handle`] but threads a synthetic
    /// `spawn_key` onto the spawned `SessionTask`. The first
    /// `AgentEvent::Connected` arriving on the task drives a
    /// `SessionUpdate::KeyRenamed { from: spawn_key, to: real_key }`
    /// emit before the matching `Connected` so TUI re-keys its
    /// `UiSession` map atomically. `None` for re-entrant callers (the
    /// pooled handle path) where no key migration is needed.
    pub fn get_agent_handle_with_spawn_key(
        self: &Arc<Self>,
        target: SessionTarget,
        settings: SessionLaunchSettings,
        spawn_key: Option<SessionKey>,
    ) -> Result<Arc<AgentHandle>> {
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
        let (account_key, account_dir) =
            self.plan_assignment(&target, spawn_key.as_ref()).unwrap_or_else(|| {
                let project_account_pin = self.project_accounts_for(&target);
                let accounts = self.accounts.lock();
                accounts.pick_for_project(&project_account_pin)
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
        let account_proxy_enabled = self.accounts.lock().proxy_enabled(&account_key);
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
            //     peer_prompt pre-populated pending_peer_prompts /
            //     current_inbound_hop at synth_key=`__spawn_<name>__`
            //     before dispatching SpawnProject). Move that
            //     DomainSession onto `session_key` so the SessionTask
            //     we're about to construct sees the buffered state.
            //  3. Neither - create fresh at `session_key`.
            //
            // When both `session_key` and `spawn_key` exist (race:
            // peer ask arrives while a pre-Connect placeholder was
            // already there), merge `spawn_key`'s buffered prompts
            // / hop into the placeholder. The placeholder is the
            // one the SessionTask will pick up via `session_key`.
            if let Some(existing) = handles.get(&session_key).cloned() {
                if let Some(spawn) = spawn_key.as_ref()
                    && spawn != &session_key
                    && let Some(buffered) = handles.remove(spawn)
                {
                    let mut placeholder = existing.lock();
                    let mut src = buffered.lock();
                    placeholder.pending_peer_prompts.append(&mut src.pending_peer_prompts);
                    if let Some(hop) = src.current_inbound_hop {
                        let current = placeholder.current_inbound_hop.unwrap_or(0);
                        placeholder.current_inbound_hop = Some(current.max(hop));
                    }
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
            let resolver = crate::mcp::peers::facade::CallerKeyResolver::from_domain(&domain_arc);
            crate::mcp::build_forge_server(workspace_facade, worker_facade, resolver, session_kind)
        };

        let handle = forge_agent::Agent::spawn(
            account_dir.clone(),
            Some(account_key.0.clone()),
            attached_proxy,
            vec![("forge".to_owned(), forge_server)],
        );
        // Project-rooted targets (`Default` / `Named`) resume the
        // project's lead session when the on-disk catalog has one,
        // and fall back to a fresh session in that project's cwd
        // otherwise. Pool key = lead's session id from the catalog
        // so it stays consistent with the running session id.
        match target {
            SessionTarget::Default => {
                let project = self.config.default_project();
                let cwd = project.path.to_string_lossy().to_string();
                if let Some(lead) = self.try_lead_session_id_for(project) {
                    handle.resume_or_new_session(lead.as_str().to_owned(), cwd, settings)?;
                } else {
                    handle.new_session(cwd, settings)?;
                }
            }
            SessionTarget::Named(name) => {
                let project = self.find_project_by_name(&name)?;
                let cwd = project.path.to_string_lossy().to_string();
                if let Some(lead) = self.try_lead_session_id_for(project) {
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
                        path: self.config_dir.join("forge.toml"),
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
                connected_once: false,
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
    /// `pending_peer_prompts` buffered at the synthetic `spawn_key`
    /// (e.g. `__spawn_<project>__`) into the live session via
    /// `Command::Prompt`. Without this, peer asks aimed at a
    /// running-but-pre-spawn-dispatched target strand at the synth
    /// key forever - the regular Connected-time drain only fires
    /// when a fresh SessionTask boots.
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
        let (pending, incoming_hop) = {
            let mut guard = buffered_domain.lock();
            (std::mem::take(&mut guard.pending_peer_prompts), guard.current_inbound_hop)
        };
        if pending.is_empty() && incoming_hop.is_none() {
            return;
        }
        // Stamp the inbound hop on the live session, taking max
        // against any existing value so concurrent inbound asks
        // don't undershoot each other.
        if let Some(hop) = incoming_hop
            && let Some(live) = self.domain_handles.lock().get(session_key).cloned()
        {
            let mut guard = live.lock();
            let current = guard.current_inbound_hop.unwrap_or(0);
            guard.current_inbound_hop = Some(current.max(hop));
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
    #[must_use]
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
    #[must_use]
    pub fn project_has_assigned_account(&self, project_key: &ProjectKey) -> bool {
        let plan = self.assignment_plan.lock();
        plan.as_ref().is_some_and(|p| !p.project_has_no_assignments(project_key))
    }

    /// Snapshot of `(AccountKey display name, LoadingState)` pairs in
    /// declaration order. Forge-tui's launchpad renders the per-
    /// account loading glyph row from this; the order matches
    /// `forge.toml`'s `[[accounts]]` declarations so the glyphs sit
    /// next to the user's mental model of which-account-is-which.
    #[must_use]
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
    /// - `Ready` + 5h `UsageWindow::is_currently_limited` true ->
    ///   `FiveHourCap` (yellow; the session still runs but the
    ///   user should know they're inside the 5h budget cap window).
    /// - Otherwise -> `Normal` (DIM; default chip).
    #[must_use]
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
        let usage = accounts.usage(&account_key).cloned();
        drop(accounts);

        let state = match loading {
            crate::account::LoadingState::Bailed => SessionChipState::Bailed,
            crate::account::LoadingState::Ready => {
                let limited = usage
                    .as_ref()
                    .and_then(|u| u.five_hour.as_ref())
                    .is_some_and(forge_primitives::usage::UsageWindow::is_currently_limited);
                if limited { SessionChipState::FiveHourCap } else { SessionChipState::Normal }
            }
            _ => SessionChipState::Normal,
        };

        Some(SessionChipInfo { account_name: account_key.0, state })
    }

    /// Crate-internal accessor for the assignment plan. Returns the
    /// `Mutex<Option<...>>` so callers (the launchpad render path
    /// and the spawn-path integration in §2.5) can take the lock
    /// briefly without paying for an Option clone. `None` means
    /// the boot-time loading tasks haven't all reached terminal
    /// yet; `Some` means the plan is live and the launchpad can
    /// un-dim project rows.
    // Callers land in Sections 2.5 / 3 / 4 of #246. Temporary
    // `dead_code` allow until those commits land within the same PR.
    #[allow(dead_code)]
    pub(crate) fn assignment_plan(&self) -> &Mutex<Option<crate::assignment_plan::AssignmentPlan>> {
        &self.assignment_plan
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
    pub(crate) fn extend_plan_for_adhoc_worker(&self, project_key: &ProjectKey, label: &str) {
        let mut plan_guard = self.assignment_plan.lock();
        let Some(plan) = plan_guard.as_mut() else {
            return;
        };
        plan.assign_adhoc_worker(project_key, &label.to_owned());
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

        let ready_accounts: Vec<AccountKey> = {
            let accounts = self.accounts.lock();
            if !accounts.all_loaded() {
                return;
            }
            accounts
                .by_key
                .iter()
                .filter(|(_, s)| matches!(s.loading, LoadingState::Ready))
                .map(|(k, _)| k.clone())
                .collect()
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
                team: p.team.clone(),
            })
            .collect();

        let fresh = compute_plan(&ready_accounts, &projects);
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
        let entries: Vec<(AccountKey, std::path::PathBuf)> = {
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
                .filter_map(|key| accounts.config_dir(key).map(|dir| (key.clone(), dir.clone())))
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
            for (key, _) in &entries {
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
        for (key, dir) in entries {
            let fetch_result = forge_agent::cloud::oauth_usage::oauth_usage(&dir).await;
            match fetch_result {
                Ok(payload) => match forge_agent::cloud::oauth::snapshot_from_payload(payload) {
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
                },
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
            let config_dir = self.config_dir.clone();
            // toml::to_string_pretty + std::fs::write/rename are sync;
            // hop to spawn_blocking so a slow disk doesn't park a tokio
            // worker (file is tiny so latency is sub-ms on a healthy
            // disk, but the pattern is wrong otherwise).
            let join = tokio::task::spawn_blocking(move || {
                crate::account_cache::store(&config_dir, &snapshots);
            })
            .await;
            if let Err(err) = join {
                tracing::warn!(
                    target: "forge_workspace::account_cache",
                    error = %err,
                    "account_cache::store spawn_blocking task panicked",
                );
            }
            tracing::info!(
                target: "forge_workspace::account_cache",
                event_name = "account_cache_written",
                accounts = account_count,
                path = %crate::account_cache::state_path(&self.config_dir).display(),
                "forge-state.toml updated after successful poll round",
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
    /// catalog for the session's original cwd and match against
    /// `LoadedProject.path` so a resumed session inherits the
    /// originating project's pin.
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
                    let cwd_path = std::path::PathBuf::from(&cwd);
                    self.config
                        .projects
                        .iter()
                        .find(|p| p.path == cwd_path)
                        .map(|p| p.accounts.clone())
                });
                matched.unwrap_or_else(|| self.config.default_project().accounts.clone())
            }
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

    /// Look up a project by `name` from `forge.toml`. Returns
    /// [`WorkspaceError::ProjectNotFound`] when no project carries
    /// that name.
    fn find_project_by_name(&self, name: &str) -> Result<&LoadedProject, WorkspaceError> {
        self.config.projects.iter().find(|project| project.name == name).ok_or_else(|| {
            WorkspaceError::ProjectNotFound {
                name: name.to_owned(),
                path: self.config_dir.join("forge.toml"),
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
        let key = ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(
            Some(cwd),
        ));
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
    /// log) any label whose files are missing  -  so a single bad label
    /// in `team = [...]` doesn't block the rest of the team.
    fn load_team_roles(
        team: &[String],
        project_key: &crate::target::ProjectKey,
    ) -> Vec<crate::team::Role> {
        let mut loaded: Vec<crate::team::Role> = Vec::with_capacity(team.len());
        for label in team {
            match crate::team::Role::load(label) {
                Ok(role) => loaded.push(role),
                Err(err) => {
                    tracing::warn!(
                        target: "forge_workspace::team",
                        project = %project_key.as_str(),
                        label = %label,
                        error = %err,
                        "no charter / kick file found for worker label; spawn skipped. Populate ~/.claude/forge-team/<label>/charter.md and kick.md (copy from docs/forge-team-defaults/<label>/) or use the workers__create_role MCP tool."
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
        team: Vec<String>,
    ) {
        if team.is_empty() {
            return;
        }
        if !self.try_claim_team_spawn(&project_key) {
            tracing::debug!(
                target: "forge_workspace::team",
                project = %project_key.as_str(),
                "team-spawn already in flight; skipping duplicate Connected fire",
            );
            return;
        }
        // When invoked inside a tokio runtime (production + any
        // `#[tokio::test]`), spawn the catalog scan + dispatch
        // asynchronously so translate_event isn't blocked on file
        // I/O. When invoked outside a runtime (the sync `#[test]`
        // fixtures in `team_hook_tests`), fall back to a synchronous
        // dispatch with an empty resume map: those tests exercise
        // the role-fanout shape, not the resume mechanic. Tests that
        // need the resume path opt into `#[tokio::test]` + fixture
        // JSONLs explicitly.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                target: "forge_workspace::team",
                project = %project_key.as_str(),
                "no tokio runtime in scope; falling back to sync team-spawn (test path)",
            );
            let loaded = Self::load_team_roles(&team, &project_key);
            self.spawn_team_for_lead_with_resume(
                &lead_session_id,
                &project_key,
                &loaded,
                &std::collections::HashMap::new(),
            );
            self.release_team_spawn(&project_key);
            return;
        };
        let workspace = Arc::clone(self);
        let config_dir = self.config_dir.clone();
        handle.spawn(async move {
            let resume_map = scan_worker_resume_map(&config_dir, &project_dir).await;
            let loaded = Self::load_team_roles(&team, &project_key);
            tracing::info!(
                target: "forge_workspace::team",
                project = %project_key.as_str(),
                lead_session_id = %lead_session_id,
                resume_count = resume_map.len(),
                fresh_count = loaded.len().saturating_sub(resume_map.len()),
                missing_count = team.len().saturating_sub(loaded.len()),
                "team-spawn catalog scan complete; dispatching SpawnWorker per label",
            );
            workspace.spawn_team_for_lead_with_resume(
                &lead_session_id,
                &project_key,
                &loaded,
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
    #[must_use]
    pub(crate) fn worker_has_progress_past_kick(&self, session_id: &str) -> bool {
        let messages = forge_agent::userdata::catalog::scan::get_session_messages(
            &self.config_dir,
            session_id,
            None,
        );
        let user_turn_count = messages
            .iter()
            .filter(|m| matches!(m.kind, forge_primitives::SessionMessageKind::User))
            .count();
        user_turn_count >= 2
    }

    /// Park an oneshot in
    /// `DomainSession.pending_interactions[tool_id]`. Called from
    /// `SessionTask::run` when an `AgentEvent::PermissionRequest` /
    /// `QuestionRequest` arrives.
    ///
    /// No-op when no `SessionTask` is registered for `key` (e.g.,
    /// the session was just closed) - the oneshot is dropped and the
    /// caller's forwarder task observes a closed receiver, which
    /// surfaces as an `oneshot::Recv` error in the existing
    /// permission/question response forwarder paths.
    pub fn store_pending_interaction(
        &self,
        key: &SessionKey,
        tool_id: String,
        slot: PendingInteractionSlot,
    ) {
        let Some(domain) = self.domain_handles.lock().get(key).cloned() else {
            tracing::warn!(
                target: "forge_workspace",
                key = %key.as_str(),
                tool_id = %tool_id,
                "store_pending_interaction: no domain handle for key (session may be closed)",
            );
            return;
        };
        let mut guard = domain.lock();
        guard.pending_interactions.insert(tool_id, slot);
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

    // ---- Live workers (project-internal child-agent coordination) ----

    /// Snapshot the live workers for `project_key`. Returns an empty
    /// Vec when no workers exist (rather than `None`) so the TUI tree-
    /// child render can branch only on `is_empty`.
    #[must_use]
    pub fn list_live_workers(
        &self,
        project_key: &ProjectKey,
    ) -> Vec<crate::mcp::workers::types::WorkerEntry> {
        self.live_workers.lock().get(project_key).cloned().unwrap_or_default()
    }

    /// Snapshot every live worker's session_key across every project.
    /// Used by the TUI's `find_running_bucket_for_path` to exclude
    /// worker buckets from project-row click routing without depending
    /// on `list_projects()` for enumeration.
    #[must_use]
    pub fn all_live_worker_session_keys(&self) -> Vec<SessionKey> {
        self.live_workers
            .lock()
            .values()
            .flat_map(|entries| entries.iter().map(|e| e.session_key.clone()))
            .collect()
    }

    /// Insert a worker entry into `live_workers[project_key]`. Duplicate
    /// labels are allowed; addressing by label uses latest-spawned wins
    /// (see `remove_latest_worker`).
    pub fn insert_live_worker(
        &self,
        project_key: &ProjectKey,
        entry: crate::mcp::workers::types::WorkerEntry,
    ) {
        self.live_workers.lock().entry(project_key.clone()).or_default().push(entry);
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

    /// Resolve the cwd to pass to `claude --resume` for the session
    /// at `session_key`. Three-step fallback:
    /// 1. `session_cwd_for(key)` - the catalog scan returns the
    ///    original cwd from the session's `system/init` row. Works
    ///    for lead sessions; returns None for worker-tagged sessions
    ///    (the catalog walk excludes them).
    /// 2. Worker fallback (#245 Layer B): when the session is a live
    ///    worker, compose the cwd via [`worker_tag_dir`] - the same
    ///    helper that decided where claude wrote the JSONL at spawn
    ///    time. For git-repo workers that's
    ///    `<project_root>/.claude/worktrees/<label>`; for non-git
    ///    workers it's `<project_root>` unmodified.
    ///
    ///    Critically, `claude --resume` does NOT receive a
    ///    `--worktree` flag (see `SessionLaunchSettings::extra_args`
    ///    in `forge-agent/src/client.rs` - lead/resume paths leave
    ///    extra_args empty), so the subprocess cwd is the ONLY signal
    ///    claude uses to derive the JSONL location. Passing just the
    ///    project root for a git worker makes claude look under the
    ///    project's sanitised dir, miss the worker JSONL (which lives
    ///    under the worktree's sanitised dir), and exit with "No
    ///    conversation found with session ID:". Composing with
    ///    `worker_tag_dir` gives claude the right anchor so it
    ///    resolves the worker JSONL on the first try.
    /// 3. Default to empty string. Pass through and let the bridge
    ///    surface ConnectionFailed - the session can't be resumed
    ///    cleanly anyway. Logs a warn so a regression is visible in
    ///    the field instead of silently failing later.
    ///
    /// [`worker_tag_dir`]: crate::mcp::workers::types::worker_tag_dir
    pub(crate) fn resume_cwd_for_session(&self, session_key: &SessionKey) -> String {
        if let Some(cwd) = self.session_cwd_for(session_key) {
            return cwd;
        }
        if let Some((project_key, label, is_git)) = self.worker_lookup_for_session(session_key)
            && let Some(root) = self.project_root_for_key(&project_key)
        {
            return crate::mcp::workers::types::worker_tag_dir(&root, &label, is_git)
                .to_string_lossy()
                .into_owned();
        }
        tracing::warn!(
            target: "forge_workspace::workspace",
            session_key = %session_key.as_str(),
            "resume_cwd_for_session: no catalog cwd and no live worker entry; \
             passing empty cwd to claude (resume will fail with ConnectionFailed)",
        );
        String::new()
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
    #[must_use]
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
                    sender_name: entry.label.clone(),
                    sender_org: String::new(),
                    hop: 1,
                    hop_limit: 10,
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
        });
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
        });
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

    /// Persist a settings document via the bridge for `key`.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a human-readable message when no agent is
    /// registered for `key` or when the bridge's I/O fails.
    pub fn write_settings_document(
        &self,
        key: &SessionKey,
        target: &forge_agent::userdata::settings::SettingsTarget,
        document: &serde_json::Value,
    ) -> Result<(), String> {
        let handle = self
            .agent_handle_for(key)
            .ok_or_else(|| "no agent registered for session".to_owned())?;
        handle.write_settings_document(target, document).map_err(|err| err.to_string())
    }

    /// Resolve the agent's configured config_dir for `key`. Returns
    /// `None` when no agent is registered for `key`.
    pub fn config_dir_for(&self, key: &SessionKey) -> Option<PathBuf> {
        let handle = self.agent_handle_for(key)?;
        Some(handle.config_dir())
    }

    /// Fetch the OAuth usage payload via the bridge bound to `key`.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a human-readable message when no agent is
    /// registered for `key`; otherwise propagates the bridge's
    /// `OauthUsageError`.
    pub async fn oauth_usage(
        &self,
        key: &SessionKey,
    ) -> Result<forge_agent::cloud::oauth_usage::OauthUsage, String> {
        let handle = self
            .agent_handle_for(key)
            .ok_or_else(|| "Bridge connection required for OAuth usage fetch.".to_owned())?;
        handle.oauth_usage().await.map_err(|err| err.to_string())
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
fn classify_oauth_usage_error(
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
        true
    }

    /// Expire every in-flight ask whose target session is the one
    /// closing. Called when:
    /// - `AgentEvent::ConnectionFailed` arrives for a target's bridge
    ///   (target's claude subprocess crashed or failed to spawn)
    /// - A `SessionTask::drop` fires (target's session was closed by
    ///   any reason - user close, lifecycle terminate, panic)
    ///
    /// Maps the closing `SessionKey` to its project name via
    /// `list_projects`, then walks `inflight_asks` for entries whose
    /// `target_project` matches and dispatches the failure dual-path
    /// notification for each (PeerAskFailed UI state + Command::Prompt
    /// with DeliveryFailureNotice wrapper to caller).
    ///
    /// Idempotent. Safe to call from a Drop impl via Weak<Workspace>.
    pub(crate) fn expire_target_inflight(
        self: &Arc<Self>,
        closing_key: &SessionKey,
        reason: crate::mcp::peers::types::PeerFailureReason,
    ) {
        // Find the project this closing session belongs to. If we can't
        // (caller raced with config-reload; defensive only), there's
        // nothing to do.
        let project_name = self
            .list_projects()
            .into_iter()
            .find(|v| v.sessions.iter().any(|s| s.session == *closing_key))
            .map(|v| v.name);
        let Some(project_name) = project_name else {
            return;
        };

        // Snapshot the IDs to expire. Holding the inflight_asks lock
        // across the dispatch loop below would risk re-entrancy via
        // bump_inflight_stats. Take a copy + release the lock.
        let ids_to_expire: Vec<CorrelationId> = {
            let asks = self.inflight_asks.lock();
            asks.iter()
                .filter(|(_, ask)| ask.target_project == project_name)
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
            sender_name: ask.target_project.clone(),
            sender_org: target_org,
            hop: 0,
            hop_limit: 10,
            body,
        };
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

#[cfg(any(test, feature = "testing"))]
impl Workspace {
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
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let (kick_dispatcher_tx, kick_dispatcher_rx) = mpsc::unbounded_channel::<KickRequest>();
        let workspace = Self {
            config_dir,
            config: LoadedConfig::empty_for_test(),
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
            usage_poller_started: std::sync::atomic::AtomicBool::new(false),
            kick_dispatcher_tx,
            kick_dispatcher_rx_slot: Mutex::new(Some(kick_dispatcher_rx)),
            // testing_stub skips Workspace::new and therefore the
            // proxy boot. Tests don't drive real subprocesses.
            proxy: None,
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
    /// to drive the Connected-hook team-spawn trigger without
    /// writing a real `forge.toml`. Test-only.
    pub fn seed_test_project_with_team(&self, name: &str, path: &str, team: &[String]) {
        self.test_extra_projects.lock().push(crate::config::LoadedProject {
            name: name.to_owned(),
            path: std::path::PathBuf::from(path),
            display_path: path.to_owned(),
            org: "TestOrg".to_owned(),
            accounts: vec!["acct-a".to_owned()],
            auto_start: false,
            team: team.to_vec(),
        });
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

    fn make_workspace_dir() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("forge.toml"),
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
    async fn get_agent_handle_default_is_idempotent() {
        let dir = make_workspace_dir();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
            dir.path().join("forge.toml"),
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

        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
            dir.path().join("forge.toml"),
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

        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
            dir.path().join("forge.toml"),
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

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
            dir.path().join("forge.toml"),
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

        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
            dir.path().join("forge.toml"),
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

        let caller = SessionKey::from_str_for_test("caller-1");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                caller: caller.clone(),
                caller_project: "forge".to_owned(),
                target_project: "granite-backend".to_owned(),
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
    /// expire_target_inflight is a thin loop over this per-id path; in
    /// production it resolves the closing session's project via
    /// `list_projects` (catalog-backed), so an in-memory test that
    /// never writes to disk would early-return on project lookup. We
    /// exercise the per-id unit instead.
    #[tokio::test]
    async fn expire_inflight_ask_failed_dispatches_failure_notice() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk, PeerFailureReason};
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionKey::from_str_for_test("caller-notice");
        let id = CorrelationId::new_ask();
        workspace.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                caller: caller.clone(),
                caller_project: "forge".to_owned(),
                target_project: "granite-backend".to_owned(),
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

    /// Workspace::dispatch(Command::DeliverPeerPrompt) routes to the
    /// command channel without panicking. The full spawn-path handling
    /// is exercised in the spawn::handle_deliver_peer_prompt test.
    #[tokio::test]
    async fn dispatch_command_deliver_peer_prompt_routes_cleanly() {
        use crate::mcp::peers::types::{CorrelationId, WrappedKind, WrappedPrompt};
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

        let caller = SessionKey::from_str_for_test("caller-dispatch");
        let wrapped = WrappedPrompt {
            correlation_id: CorrelationId::new_tell(),
            kind: WrappedKind::Message,
            sender_name: "forge".to_owned(),
            sender_org: "Default".to_owned(),
            hop: 1,
            hop_limit: 10,
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

    /// Disk-backed workspace fixture shared by the per-project loop
    /// tests below. Returns the `Arc<Workspace>` plus the `TempDir`
    /// that holds the on-disk `forge.toml`; the caller must keep the
    /// `TempDir` alive (drop deletes the directory). Required because
    /// `expire_target_inflight` resolves the closing key's project via
    /// `list_projects()` (catalog-backed), so a fully-in-memory
    /// workspace would early-return.
    async fn peer_mcp_workspace_fixture() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = forge_toml_with_two_projects();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
                    caller: caller_a.clone(),
                    caller_project: "forge".to_owned(),
                    target_project: "granite-backend".to_owned(),
                },
            );
            asks.insert(
                id_b.clone(),
                InflightAsk {
                    correlation_id: id_b.clone(),
                    caller: caller_b.clone(),
                    caller_project: "forge".to_owned(),
                    target_project: "granite-backend".to_owned(),
                },
            );
            asks.insert(
                id_c.clone(),
                InflightAsk {
                    correlation_id: id_c.clone(),
                    caller: caller_c.clone(),
                    caller_project: "granite-backend".to_owned(),
                    target_project: "forge".to_owned(),
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
        }
    }

    fn make_workspace_dir() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("forge.toml"),
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));

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
            tag: tag.map(str::to_owned),
            created_at: None,
        }
    }

    /// Regression for the reviewer-flagged miss on PR #164: workers
    /// spawned with `--worktree=<label>` `chdir` into
    /// `<project>/.claude/worktrees/<label>/` which is indexed under
    /// a SIBLING `<config_dir>/projects/<sanitize(worktree_path)>/`
    /// subdir. A `directory=Some(<project>)` scan misses them. The
    /// cwd-prefix filter catches them because every worktree cwd
    /// starts with `<project>`.
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
        let map = build_resume_map_from_sessions(&sessions, project_dir);
        assert_eq!(map.len(), 2, "only worker-tagged sessions land in the map");
        assert_eq!(map.get("planner"), Some(&"planner-uuid".to_owned()));
        assert_eq!(map.get("reviewer"), Some(&"reviewer-uuid".to_owned()));
    }

    /// Workers from OTHER projects must NOT appear in this project's
    /// resume map - their cwd doesn't start with `project_dir`.
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
        let map = build_resume_map_from_sessions(&sessions, project_dir);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("planner"), Some(&"ours".to_owned()));
    }

    /// Path-prefix matching must be component-aware: a project at
    /// `/Users/me/Projects/forge` MUST NOT match workers from a
    /// sibling project at `/Users/me/Projects/forge-old` whose cwd
    /// shares the `forge` byte-prefix. `Path::starts_with` (not
    /// `str::starts_with`) handles this correctly. Without the
    /// component-aware check, `forge-old`'s workers would silently
    /// migrate into `forge`'s resume map - same bug class as #157.
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
        let map = build_resume_map_from_sessions(&sessions, project_dir);
        assert_eq!(map.len(), 1, "only the matching-project worker should resume");
        assert_eq!(map.get("planner"), Some(&"ours".to_owned()));
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
        let map = build_resume_map_from_sessions(&sessions, project_dir);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("planner"));
    }

    /// Non-git project: workers run in the project's main cwd (no
    /// worktree). The cwd-prefix filter still catches them since
    /// project's main cwd == project_dir.
    #[test]
    fn build_resume_map_finds_workers_in_non_git_project() {
        let project_dir = std::path::Path::new("/Users/me/Projects/non-git");
        let sessions = vec![mk_info(
            "tester-uuid",
            Some("/Users/me/Projects/non-git"),
            Some("forge:worker:tester"),
        )];
        let map = build_resume_map_from_sessions(&sessions, project_dir);
        assert_eq!(map.get("tester"), Some(&"tester-uuid".to_owned()));
    }

    /// Sessions with no `cwd` field (uncommon - filesystem write
    /// race?) are ignored rather than panic.
    #[test]
    fn build_resume_map_skips_sessions_with_no_cwd() {
        let project_dir = std::path::Path::new("/Users/me/Projects/forge");
        let sessions = vec![mk_info("orphan", None, Some("forge:worker:planner"))];
        let map = build_resume_map_from_sessions(&sessions, project_dir);
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
        let map = build_resume_map_from_sessions(&sessions, project_dir);
        assert_eq!(map.get("planner"), Some(&"newer".to_owned()));
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
    /// message must NOT dispatch a lead-notice. Behaviour was changed
    /// in #245: previously the entry was rolled back (removed); now it
    /// transitions to `WorkerLiveness::Failed` with the message as
    /// diagnostic, so the user sees the failure surfaced on the row
    /// rather than the worker silently vanishing.
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
        ws.seed_test_project_with_team(project_name, project_root, &[]);
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
        ws.seed_test_project_with_team("forge", "/tmp/test-forge-lead", &[]);
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
            dir.path().join("forge.toml"),
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
    async fn recompute_plan_if_ready_noop_while_loading() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        // Fresh workspace: account starts in `Loading`. all_loaded
        // returns false; recompute must not populate the plan.
        workspace.recompute_plan_if_ready();
        let plan = workspace.assignment_plan().lock();
        assert!(plan.is_none(), "plan stays None while accounts are still Loading");
    }

    #[tokio::test]
    async fn recompute_plan_if_ready_populates_plan_when_all_ready() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let plan = workspace.assignment_plan().lock();
        let plan = plan.as_ref().expect("plan populates once all_loaded fires");
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                &dir.path().join("..").join("Projects").join("forge").to_string_lossy(),
            )));
        // Don't assert the exact project_key (path expansion is
        // env-dependent); just assert SOMETHING got assigned to
        // the lone project.
        let _ = project_key;
        assert!(!plan.is_empty(), "plan must have at least one assignment for the lone project");
    }

    #[tokio::test]
    async fn recompute_plan_if_ready_uses_frozen_overlay_on_recompute() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        let first_plan = workspace.assignment_plan().lock().clone();

        // Recompute should be idempotent on the same ready set
        // (frozen overlay merges; existing assignments preserved).
        workspace.recompute_plan_if_ready();
        let second_plan = workspace.assignment_plan().lock().clone();
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
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        workspace.extend_plan_for_adhoc_worker(&project_key, "reviewer");
        assert!(workspace.assignment_plan().lock().is_none(), "plan still unpopulated");
    }

    #[tokio::test]
    async fn extend_plan_for_adhoc_worker_extends_when_plan_populated() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
        workspace.extend_plan_for_adhoc_worker(&project_key, "reviewer");
        let plan = workspace.assignment_plan().lock();
        let plan = plan.as_ref().expect("populated");
        assert!(
            plan.lookup(&project_key, &"reviewer".to_owned()).is_some(),
            "extend_plan_for_adhoc_worker adds the adhoc label to the plan",
        );
    }

    #[tokio::test]
    async fn session_chip_for_returns_none_when_plan_unpopulated() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let project_key =
            ProjectKey::new(forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
                workspace.config.projects[0].path.to_string_lossy().as_ref(),
            )));
        assert!(workspace.session_chip_for(&project_key, "lead").is_none());
    }

    #[tokio::test]
    async fn session_chip_for_normal_branch_for_ready_account() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
    async fn session_chip_for_five_hour_cap_branch() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        {
            let mut accounts = workspace.account_states().lock();
            let snapshot = forge_primitives::usage::UsageSnapshot {
                source: forge_primitives::usage::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                // 5h window at 100% with future resets_at -> FiveHourCap branch.
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
        assert_eq!(chip.state, SessionChipState::FiveHourCap);
    }

    #[tokio::test]
    async fn session_chip_for_bailed_branch() {
        let dir = make_workspace_dir_246();
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
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
