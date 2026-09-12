//! Test-only constructors, seed helpers and dispatch interception for
//! `Workspace`, so tests here and in `forge-tui` can drive a workspace
//! without a real subprocess.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::account::{AccountKey, AccountStateMap};
use crate::config::LoadedConfig;
use crate::protocol::SessionUpdate;
use crate::target::{ProjectKey, SessionKey};
use crate::workspace::{KickRequest, Workspace};

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
        // Built from the injected config the way `new` builds it, so a
        // test that supplies a `[[slack]]` entry gets a workspace whose
        // clients exist. `new` cannot fail here on a stub config.
        let slack = Arc::new(
            crate::slack::SlackWorkspaces::from_config(&config.slack, &reqwest::Client::new())
                .unwrap_or_default(),
        );
        Self::testing_stub_with_slack(config_dir, config, slack)
    }

    /// [`Self::testing_stub_with_config`] with the Slack clients supplied
    /// by the caller, so a test can drive a real facade against a double
    /// rather than a live workspace.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn testing_stub_with_slack(
        config_dir: PathBuf,
        config: LoadedConfig,
        slack: Arc<crate::slack::SlackWorkspaces>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<SessionUpdate>) {
        // Mirror the boot-time `ensure_forge_data_dir`: stub-based tests
        // that exercise the cron / state stores expect `forge/` present.
        let _ = crate::config::ensure_forge_data_dir(&config_dir);
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let (kick_dispatcher_tx, kick_dispatcher_rx) = mpsc::unbounded_channel::<KickRequest>();
        let config_dictate = config.dictate.clone();
        let workspace = Self {
            config_dir,
            config,
            catalog: Arc::new(Mutex::new(HashMap::new())),
            pool: Mutex::new(HashMap::new()),
            accounts: Mutex::new(AccountStateMap::empty_for_test()),
            assignment_plan: Mutex::new(None),
            dictate: Arc::new(crate::dictate::DictateState::new(&config_dictate)),
            dictate_runtime: Mutex::new(crate::dictate::DictateRuntime::default()),
            dictate_device_pick: Mutex::new(None),
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
            _single_instance_lock: None,
            crons: Mutex::new(Vec::new()),
            pending_cron_by_owner: Mutex::new(HashMap::new()),
            gotify_subs: Mutex::new(Vec::new()),
            db: Arc::new(Mutex::new(None)),
            catalog_loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            catalog_ready_notify: Arc::new(tokio::sync::Notify::new()),
            catalog_scan_started: std::sync::atomic::AtomicBool::new(false),
            gotify_connected: Mutex::new(false),
            gotify_app_index: Mutex::new(HashMap::new()),
            gotify_subsystem: Mutex::new(None),
            slack,
            slack_subs: Mutex::new(Vec::new()),
            slack_subsystem: Mutex::new(std::collections::BTreeMap::new()),
            slack_connected: Mutex::new(std::collections::BTreeMap::new()),
            slack_user_ids: Mutex::new(std::collections::BTreeMap::new()),
            slack_drafts: Mutex::new(HashMap::new()),
            slack_recently_delivered: Mutex::new(HashMap::new()),
            slack_load_failed: std::sync::atomic::AtomicBool::new(false),
            slack_user_id_retries: Mutex::new(std::collections::BTreeMap::new()),
            slack_verification_started: std::sync::atomic::AtomicBool::new(false),
            respawn_in_flight: Mutex::new(std::collections::HashSet::new()),
            command_intercept: Mutex::new(None),
            test_extra_projects: Mutex::new(Vec::new()),
        };
        (Arc::new(workspace), update_rx)
    }
}

/// Test-mode command interception. Feature-gated and `pub` for the same
/// reason the seeds below are: forge-tui's tests reach for these to
/// assert what boot WOULD have spawned, without starting a subprocess.
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
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Append a synthetic project to the test overlay searched first
    /// by `find_project_view_by_name`. Used by respawn tests
    /// to drive the Connected-hook worker-spawn trigger without
    /// writing a real `forge.toml`. Test-only.
    pub fn seed_test_project(&self, name: &str, path: &str) {
        self.test_extra_projects.lock().push(crate::config::LoadedProject {
            name: name.to_owned(),
            path: std::path::PathBuf::from(path),
            display_path: path.to_owned(),
            org: "TestOrg".to_owned(),
            accounts: vec!["acct-a".to_owned()],
            fallback_accounts: Vec::new(),
            auto_start: false,
            env: std::collections::HashMap::new(),
            max_workers: None,
        });
    }

    /// Mark `account` Ready and recompute the assignment plan, so a
    /// cross-crate test can render chip-bearing rows without driving the
    /// real account loader. Test-only.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_test_ready_account(&self, account: &str) {
        self.accounts
            .lock()
            .set_loading(&AccountKey(account.to_owned()), crate::account::LoadingState::Ready);
        self.recompute_plan_if_ready();
    }

    /// Drive `account` to `state` directly, so a cross-crate test can
    /// render a mid-flight or bailed preflight screen without the real
    /// loader. Test-only.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_test_account_state(&self, account: &str, state: crate::account::LoadingState) {
        self.accounts.lock().set_loading(&AccountKey(account.to_owned()), state);
    }

    /// Record a probe failure on `account`, so a cross-crate test can
    /// render bailed-row copy keyed on why the account bailed. Test-only.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_test_account_failure(
        &self,
        account: &str,
        status: crate::account::UsageFetchStatus,
    ) {
        self.accounts.lock().set_last_error(&AccountKey(account.to_owned()), status, None);
    }

    /// Replace the dictation preflight snapshot, so a cross-crate test
    /// can render any of its states without fetching 3 GB. Test-only.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_test_dictate_snapshot(&self, snapshot: crate::dictate::DictateSnapshot) {
        *self.dictate.snapshot.lock() = snapshot;
    }

    /// A `testing_stub` whose `[dictate] enabled` is true, so a
    /// cross-crate test exercises the key handler's enabled path
    /// without a model download. Test-only.
    #[cfg(any(test, feature = "testing"))]
    pub fn testing_stub_with_dictate_enabled() -> (Arc<Self>, mpsc::UnboundedReceiver<SessionUpdate>)
    {
        let mut config = LoadedConfig::empty_for_test();
        config.dictate.enabled = true;
        Self::testing_stub_with_config(PathBuf::from("/tmp/forge-testing-stub-dictate"), config)
    }

    /// Give `label` an assignment-plan entry the way a spawn does, so a
    /// cross-crate test can produce a chipped worker row. A label without
    /// one renders bare, which is the contrast worth testing; assignment
    /// is what puts it in the plan now that nothing pre-seeds from
    /// forge.toml. Test-only.
    pub fn seed_test_worker_assignment(&self, project_key: &ProjectKey, label: &str) {
        let _ = self.extend_plan_for_adhoc_worker(project_key, label);
    }

    /// Persist a dynamic-worker row directly, bypassing `workers__spawn`.
    /// Cross-crate test access to the otherwise `pub(crate)` store write
    /// so forge-tui can render launchpad worker rows against a seeded row.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_test_dynamic_worker(&self, project_key: &ProjectKey, label: &str) {
        let _ = self.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: project_key.as_str().to_owned(),
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            kick: None,
            resume_kick: None,
            interactive: false,
        });
    }
}
