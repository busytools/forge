//! The Slack subsystem: one client per configured workspace, the
//! subscription records sessions create, the durable sweep watermarks and
//! followed threads, the composed drafts held for a decision, and the pump's
//! life.
//!
//! # The host seams
//!
//! The workspace holds registration and routing; this module owns the state
//! machine. What it needs from its host arrives through three seams:
//!
//! - [`SlackStore`] is every read and write that must survive a restart
//!   (subscriptions, cursors, threads, the own-post ledger). A run with no
//!   store answers the no-store defaults, so the subsystem keeps one code
//!   path.
//! - [`SlackEmit`] is the frames a write or a draft produces, the
//!   announcement a subscription set moved, and the delivery of a matched
//!   batch into its subscriber's session.
//! - [`forge_primitives::AskSink`] is the ask number and settlement the host
//!   keeps; [`forge_primitives::AskSource`] is what a view reads back, and
//!   this module implements it, so a held draft cannot be drawn without
//!   being counted.
//!
//! The pump's HTTP client is a host-supplied factory ([`PumpHttp`]) because
//! the trusted roots are the agent layer's, which this crate may not depend
//! on - the same seam [`crate::gotify::GotifyHost::http_client`] is.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use forge_primitives::slack::{
    DEFAULT_DM_POLL_SECONDS, DEFAULT_POLL_SECONDS, DEFAULT_STALE_AFTER_HOURS,
    DEFAULT_THREAD_IDLE_DAYS, MIN_DM_POLL_SECONDS, SlackConfig, SlackDraft, SlackDraftAnswer,
    SlackDraftEnding, SlackFollowedThread, SlackMessage, SlackSubscription,
    SlackSubscriptionTarget, SlackThreadOwner, SlackThreadRecord, SlackWatchMode,
};
use forge_primitives::{Ask, AskSink, AskSource, PendingAsk, PendingInteractionKind, SessionSlot};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::slack::{AuthTest, MENTION_CURSOR, REQUEST_TIMEOUT, SlackApi, SlackClient, SlackHost};

/// What ended a held draft, as the registry resolves it: the reader's
/// answer, or nothing at all when the asking session went away.
pub enum DraftResolution {
    Answered(SlackDraftAnswer),
    Abandoned,
}

/// One update the subsystem produces, in its own vocabulary. The host maps
/// each onto its wire frame.
pub enum SlackUpdate {
    /// A composed message is held; the draft is the card a view draws. An
    /// emit that returns `false` for this variant means no attached client
    /// could answer it, which is what makes the caller fail closed.
    PostPending { key: SessionSlot, draft: SlackDraft },
    /// A held draft ended, with the ending every client draws.
    DraftResolved { key: SessionSlot, id: Uuid, ending: SlackDraftEnding },
}

/// Where the subsystem emits its updates and reaches the two host actions
/// that are not a wire frame of its own.
pub trait SlackEmit: Send + Sync {
    /// Emit one update. `false` means no subscriber would take it - for a
    /// held draft that no client can answer, which ends it rather than
    /// holding it for ever.
    fn emit(&self, update: SlackUpdate) -> bool;

    /// A subscription write moved `project`'s set. The frame carries both
    /// connectors' sets because they draw as one section, so the host
    /// composes it rather than this module.
    fn subscriptions_changed(&self, project: &str);

    /// Hand one conversation's matched messages to its subscriber's session,
    /// as the one block they will be read as. Returns whether delivery
    /// succeeded: a `false` means the caller did not advance the
    /// conversation cursor past this batch, so the next sweep re-delivers
    /// it instead of losing it.
    fn deliver(&self, project: &str, team_role: Option<&str>, messages: &[SlackMessage]) -> bool;
}

/// The durable half of the subsystem: every read and write that must
/// survive a restart, one method per store call.
///
/// `available` is the run-with-no-store gate: a workspace whose store could
/// not open answers `false`, and the subsystem then takes the same
/// early-return path the per-call database guard used to. Every other method
/// answers the no-store default (`Ok(())`, `Ok(None)`, an empty list,
/// `Ok(false)`) so a caller that does not gate still sees nothing rather
/// than an error.
pub trait SlackStore: Send + Sync {
    fn available(&self) -> bool;
    fn insert_subscription(&self, subscription: &SlackSubscription) -> Result<(), String>;
    fn list_subscriptions(&self) -> Result<Vec<SlackSubscription>, String>;
    fn remove_subscription(&self, id: Uuid) -> Result<bool, String>;
    fn watermark(&self, workspace: &str, conversation: &str) -> Result<Option<String>, String>;
    fn set_watermark(&self, workspace: &str, conversation: &str, ts: &str) -> Result<(), String>;
    fn remove_watermark(&self, workspace: &str, conversation: &str) -> Result<bool, String>;
    fn watermark_conversations(&self, workspace: &str) -> Result<Vec<String>, String>;
    fn thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Result<Option<SlackThreadRecord>, String>;
    fn set_thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        record: &SlackThreadRecord,
    ) -> Result<(), String>;

    /// Read-modify-write one followed thread inside the host's own lock.
    ///
    /// `update` runs once, with the row as it stands (`None` when there is
    /// none) and may change it in place: leaving `None` deletes the row (or
    /// does nothing when there was none), leaving `Some` writes it. The
    /// return is the record now stored, `None` when the row was deleted or
    /// never existed.
    ///
    /// **One critical section, not two.** A per-call read followed by a
    /// per-call write lets a teardown's prune read before a delivery's write
    /// and erase the owner that delivery just added. The workspace held one
    /// database guard across both before this state moved; this is that
    /// guard, through the port.
    fn update_thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        update: &mut dyn FnMut(&mut Option<SlackThreadRecord>),
    ) -> Result<Option<SlackThreadRecord>, String>;

    fn threads_for(
        &self,
        workspace: &str,
        conversation: &str,
    ) -> Result<Vec<(String, SlackThreadRecord)>, String>;
    fn threads_for_workspace(
        &self,
        workspace: &str,
    ) -> Result<Vec<(String, String, SlackThreadRecord)>, String>;
    fn remove_thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Result<bool, String>;
    fn own_post(&self, workspace: &str, conversation: &str, ts: &str) -> Result<bool, String>;
    fn record_own_post(
        &self,
        workspace: &str,
        conversation: &str,
        ts: &str,
        now: u64,
    ) -> Result<(), String>;
}

/// The pump's HTTP client, built by the host so the trusted roots stay with
/// the agent layer this crate does not depend on.
pub type PumpHttp = dyn Fn(Duration) -> Result<reqwest::Client, String> + Send + Sync;

/// Everything the workspace hands the subsystem when it is built.
pub struct SlackSeams {
    pub store: Arc<dyn SlackStore>,
    pub emit: Arc<dyn SlackEmit>,
    pub sink: Arc<dyn AskSink>,
    pub http: Arc<PumpHttp>,
}

/// How long a delivered message stays remembered. Long enough to cover a
/// sweep that re-runs after a 429 or a restart, short enough that the map
/// stays small.
const DELIVERY_REMEMBER: Duration = Duration::from_secs(300);

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// The dedupe key for a delivered Slack message: one destination is
/// (project, owner, conversation, ts, edited), so a lead and a worker in
/// one project never starve each other as "already delivered". The edit ts
/// is part of the identity because an edit keeps the message's own `ts`:
/// without it a correction reads as a re-run of the delivery it corrects and
/// never reaches the session.
pub type SlackDeliveryKey = (String, Option<String>, String, String, Option<String>);

/// The age of a Slack `ts` in whole days, read from its seconds prefix.
/// The fraction is cursor precision, not calendar precision; the coarse
/// prefix is all the idle check needs. An unparseable cursor reads as
/// ancient and drops the row.
fn thread_idle_days(cursor: &str) -> u64 {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|now| now.as_secs())
        .unwrap_or_default();
    let cursor_secs: u64 = cursor.split('.').next().and_then(|secs| secs.parse().ok()).unwrap_or(0);
    now.saturating_sub(cursor_secs) / SECONDS_PER_DAY
}

/// One client per configured workspace, keyed by its label.
///
/// Held as [`SlackApi`] rather than the concrete client so a test can
/// substitute a double and drive a real facade against it. `Debug` is
/// hand-written because a trait object is not, and it prints only the
/// labels: a client is not a thing worth printing.
#[derive(Default)]
pub struct SlackWorkspaces {
    clients: BTreeMap<String, Arc<dyn SlackApi>>,
}

impl std::fmt::Debug for SlackWorkspaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackWorkspaces").field("labels", &self.labels()).finish()
    }
}

impl SlackWorkspaces {
    /// Labels key the clients a project's binding resolves to and name the
    /// workspace in the Inspector, so they must be distinct and the token
    /// must be present.
    pub fn from_config(configs: &[SlackConfig], http: &reqwest::Client) -> Result<Self, String> {
        validate_configs(configs)?;
        let clients: BTreeMap<String, Arc<dyn SlackApi>> = configs
            .iter()
            .map(|config| {
                let client = Arc::new(SlackClient::new(http.clone(), config.token.clone()));
                (config.workspace.trim().to_owned(), client as Arc<dyn SlackApi>)
            })
            .collect();
        Ok(Self { clients })
    }

    /// A set built from caller-supplied APIs, so a test can drive a real
    /// facade against a double instead of a live workspace.
    #[cfg(any(test, feature = "testing"))]
    pub fn from_apis(apis: BTreeMap<String, Arc<dyn SlackApi>>) -> Self {
        Self { clients: apis }
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Every configured label, sorted (the map is ordered).
    pub fn labels(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    pub fn client(&self, label: &str) -> Option<Arc<dyn SlackApi>> {
        self.clients.get(label).cloned()
    }

    /// Prove every token at boot. A failure is reported, never fatal:
    /// one dead workspace must not stop forge from booting.
    pub async fn verify_all(&self) -> Vec<(String, Result<AuthTest, String>)> {
        let mut out = Vec::new();
        for (label, client) in &self.clients {
            let result = client.auth_test().await.map_err(|err| err.to_string());
            out.push((label.clone(), result));
        }
        out
    }
}

/// The semantic checks a `[[slack]]` list must pass. Shared by the boot's
/// client build and the config loader, so a reload refuses what a restart
/// refuses.
pub fn validate_configs(configs: &[SlackConfig]) -> Result<(), String> {
    let mut labels: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for config in configs {
        let label = config.workspace.trim();
        if label.is_empty() {
            return Err("a [[slack]] entry has an empty workspace label".to_owned());
        }
        if config.token.trim().is_empty() {
            return Err(format!("slack workspace '{label}' has an empty token"));
        }
        if !labels.insert(label) {
            return Err(format!("two [[slack]] entries share the label '{label}'"));
        }
    }
    Ok(())
}

/// The distinct projects a set of subscriptions belongs to, in the order the
/// subscriptions carry them: one announcement per project a write moved.
fn projects_of(subscriptions: &[SlackSubscription]) -> Vec<String> {
    let mut projects: Vec<String> = Vec::new();
    for sub in subscriptions {
        if !projects.contains(&sub.project) {
            projects.push(sub.project.clone());
        }
    }
    projects
}

/// One composed Slack message held for a decision: the session that asked,
/// the draft itself, and the sender the blocked Slack handler awaits.
///
/// The draft rides beside the sender for the same reason the permission and
/// question requests do - the stream carries it once, so a view that
/// attached after it landed has nothing else to draw the dock from.
struct ParkedSlackDraft {
    owner: SessionSlot,
    draft: SlackDraft,
    sender: oneshot::Sender<SlackDraftAnswer>,
}

/// The Slack subsystem's state: the clients, the subscription set each pump
/// sweeps, the durable cursors and threads, the held drafts and the pump
/// handles.
///
/// The workspace holds one `Arc` of this and implements the seams above; a
/// long-lived task (a pump, a retry) holds a `Weak` so the workspace can drop
/// while it runs.
pub struct Slack {
    clients: Arc<SlackWorkspaces>,
    /// The live `[[slack]]` list, swapped by the workspace on a reload so a
    /// workspace's idle window and stale age follow the config a boot would
    /// read.
    config: Arc<parking_lot::RwLock<Vec<SlackConfig>>>,
    store: Arc<dyn SlackStore>,
    emit: Arc<dyn SlackEmit>,
    sink: Arc<dyn AskSink>,
    http: Arc<PumpHttp>,
    /// Active Slack subscriptions (`mcp__forge__slack`). The set each
    /// workspace's pump sweeps. Durable ones are also persisted through the
    /// store and reloaded here at boot.
    slack_subs: Mutex<Vec<SlackSubscription>>,
    /// Shutdown handles for the running Slack pumps, one per workspace
    /// label. Slack has a pump per `[[slack]]` entry where Gotify has a
    /// single server and a single handle.
    slack_subsystem: Mutex<BTreeMap<String, oneshot::Sender<()>>>,
    /// Per-workspace pump liveness for the Inspector's SLACK section.
    /// Keyed by label for the same reason.
    slack_connected: Mutex<BTreeMap<String, bool>>,
    /// Conversations forge has a post in flight into, keyed
    /// `(workspace, the resolved conversation)`, each entry a stamp per
    /// live call: one post call with its ledger write, or one upload's
    /// shares read. The sweep skips a conversation while any stamp is
    /// within budget, because the message can reach history before its ts
    /// is ledgered; a stamp past the request timeout is expired, never
    /// obeyed, so a stalled call cannot silence the conversation.
    slack_inflight: Mutex<HashMap<(String, String), Vec<SlackPostStamp>>>,
    /// The authenticated user's id per workspace, resolved once by the
    /// boot `auth.test` and needed to recognise `<@U...>` mentions.
    slack_user_ids: Mutex<BTreeMap<String, String>>,
    /// Display names resolved per `(workspace, user id)`, so a message from
    /// a known author costs no `users.info` call.
    slack_user_names: Mutex<BTreeMap<(String, String), String>>,
    /// `(workspace, user id)` pairs whose failed lookup has been reported, so
    /// a persistent failure says so once rather than on every tick.
    slack_author_failures: Mutex<HashSet<(String, String)>>,
    /// `(workspace, user id)` pairs whose failed DRAFT tag lookup has been
    /// reported. Kept apart from the author set so a draft's miss does not
    /// spend the once-per-author line the sweep relies on.
    slack_draft_user_failures: Mutex<HashSet<(String, String)>>,
    /// `(workspace, conversation, parent ts)` whose failed thread read has
    /// been reported, so a persistent failure says so once rather than on
    /// every park.
    slack_thread_failures: Mutex<HashSet<(String, String, String)>>,
    /// When each followed thread was last read whole, keyed
    /// `(workspace, conversation, parent ts)`. In memory: a restart costs one
    /// extra whole read per thread, which is cheaper than a schema.
    slack_thread_full_reads: Mutex<HashMap<(String, String, String), Instant>>,
    /// Followed threads whose next walk is their first since the row was
    /// created, keyed the same way.
    slack_thread_first_walks: Mutex<HashSet<(String, String, String)>>,
    /// Cache of each thread row's persisted edit stamp, keyed the same way: an
    /// edit is measured against this, never against the reply cursor. A miss
    /// reads the row, because the stamp has to outlive the process.
    slack_thread_edit_watermarks: Mutex<HashMap<(String, String, String), String>>,
    /// Conversations whose own-post ledger could not be read this run,
    /// keyed `(workspace, conversation)`. The read is retried on every
    /// message; this only decides whether the failure is worth a line a
    /// reader sees, once rather than per message.
    slack_ledger_failures: Mutex<HashSet<(String, String)>>,
    /// Composed Slack messages held for the user's decision, keyed by
    /// draft id and carrying the session that asked. The owner is stored
    /// beside it so an answer is only ever applied by the session it was
    /// addressed to.
    slack_drafts: Mutex<HashMap<Uuid, ParkedSlackDraft>>,
    /// Slack messages handed to a session recently, keyed by
    /// `(project, owner, conversation, ts)`. A sweep re-runs a batch
    /// whenever a 429 lands mid-sweep, a watermark write fails, or the
    /// process dies between the two - this is what makes that re-run
    /// idempotent rather than a re-delivery.
    slack_recently_delivered: Mutex<HashMap<SlackDeliveryKey, Instant>>,
    /// Set at boot when the durable Slack subscriptions could not be
    /// read. The Inspector SLACK section reads it: without it the empty
    /// subscription set would hide the failure.
    slack_load_failed: std::sync::atomic::AtomicBool,
    /// The last time each workspace's user-id retry ran, so a failing
    /// `auth.test` is retried at most once a minute rather than per sweep.
    slack_user_id_retries: Mutex<BTreeMap<String, SystemTime>>,
    /// Set the first time [`Slack::start_slack_verification`] runs.
    /// Subsequent calls early-return to avoid spawning duplicate probes.
    slack_verification_started: std::sync::atomic::AtomicBool,
}

impl Slack {
    /// Build the subsystem. `subscriptions` is what the workspace loaded from
    /// the store at boot; `load_failed` is whether that read failed, so the
    /// Inspector can say the empty set is not the truth.
    pub fn new(
        clients: Arc<SlackWorkspaces>,
        config: Vec<SlackConfig>,
        subscriptions: Vec<SlackSubscription>,
        load_failed: bool,
        seams: SlackSeams,
    ) -> Self {
        Self {
            clients,
            config: Arc::new(parking_lot::RwLock::new(config)),
            store: seams.store,
            emit: seams.emit,
            sink: seams.sink,
            http: seams.http,
            slack_subs: Mutex::new(subscriptions),
            slack_subsystem: Mutex::new(BTreeMap::new()),
            slack_connected: Mutex::new(BTreeMap::new()),
            slack_inflight: Mutex::new(HashMap::new()),
            slack_user_ids: Mutex::new(BTreeMap::new()),
            slack_user_names: Mutex::new(BTreeMap::new()),
            slack_author_failures: Mutex::new(HashSet::new()),
            slack_draft_user_failures: Mutex::new(HashSet::new()),
            slack_thread_failures: Mutex::new(HashSet::new()),
            slack_thread_full_reads: Mutex::new(HashMap::new()),
            slack_thread_first_walks: Mutex::new(HashSet::new()),
            slack_thread_edit_watermarks: Mutex::new(HashMap::new()),
            slack_ledger_failures: Mutex::new(HashSet::new()),
            slack_drafts: Mutex::new(HashMap::new()),
            slack_recently_delivered: Mutex::new(HashMap::new()),
            slack_load_failed: std::sync::atomic::AtomicBool::new(load_failed),
            slack_user_id_retries: Mutex::new(BTreeMap::new()),
            slack_verification_started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The live `[[slack]]` list, for the workspace to swap on a reload.
    pub fn config_handle(&self) -> Arc<parking_lot::RwLock<Vec<SlackConfig>>> {
        Arc::clone(&self.config)
    }

    /// Whether this run configured any `[[slack]]` workspace. An empty set
    /// keeps the connector dormant.
    pub fn is_configured(&self) -> bool {
        !self.clients.is_empty()
    }

    /// A ready API for this workspace, or `None` when it is not configured.
    /// The facade's client: no per-call timeout, because a long history read
    /// or upload is not a pump request.
    pub fn api_client(&self, label: &str) -> Option<Arc<dyn SlackApi>> {
        self.clients.client(label)
    }

    /// Register a Slack subscription in the active set. Durable ones also
    /// persist through the store; ephemeral ad-hoc-worker ones stay in
    /// memory only and drop on restart.
    pub fn add_slack_subscription(&self, sub: SlackSubscription, durable: bool) {
        let project = sub.project.clone();
        if durable && let Err(error) = self.store.insert_subscription(&sub) {
            tracing::warn!(
                target: "forge_tools::slack",
                %error,
                "persisting a Slack subscription failed",
            );
        }
        self.slack_subs.lock().push(sub);
        self.emit.subscriptions_changed(&project);
    }

    /// Whether this exact message has already been handed to its
    /// destination. The owner is part of the key: a lead and a worker in
    /// one project are two destinations, and one key would starve the
    /// second as "already delivered". The edit ts is part of it too: a
    /// correction is a new delivery, while a sweep re-run is not.
    pub fn slack_delivery_seen(
        &self,
        project: &str,
        team_role: Option<&str>,
        message: &SlackMessage,
    ) -> bool {
        let key = (
            project.to_owned(),
            team_role.map(str::to_owned),
            message.conversation.clone(),
            message.ts.clone(),
            message.edited.clone(),
        );
        let mut seen = self.slack_recently_delivered.lock();
        seen.retain(|_, seen_at| seen_at.elapsed() < DELIVERY_REMEMBER);
        seen.contains_key(&key)
    }

    /// Record a hand-off so a sweep re-run drops it. Committed only after
    /// a successful hand-off: a failed dispatch leaves the keys
    /// unrecorded, so the next sweep re-delivers instead of losing the
    /// messages behind a cursor that moved on.
    pub fn slack_delivery_commit(
        &self,
        project: &str,
        team_role: Option<&str>,
        messages: &[SlackMessage],
    ) {
        let mut seen = self.slack_recently_delivered.lock();
        seen.retain(|_, seen_at| seen_at.elapsed() < DELIVERY_REMEMBER);
        for message in messages {
            let key = (
                project.to_owned(),
                team_role.map(str::to_owned),
                message.conversation.clone(),
                message.ts.clone(),
                message.edited.clone(),
            );
            seen.insert(key, Instant::now());
        }
    }

    /// Hold a composed draft and hand back its id plus the receiver the
    /// caller awaits. The draft is addressed to `caller`, so its prompt
    /// surfaces in the session that will read the reply rather than in
    /// whichever session happens to be focused.
    pub fn register_slack_draft(
        &self,
        caller: &SessionSlot,
        draft: SlackDraft,
    ) -> (Uuid, oneshot::Receiver<SlackDraftAnswer>) {
        let (sender, receiver) = oneshot::channel();
        let id = draft.id;
        self.slack_drafts
            .lock()
            .insert(id, ParkedSlackDraft { owner: caller.clone(), draft: draft.clone(), sender });
        let answerable = self.emit.emit(SlackUpdate::PostPending { key: caller.clone(), draft });
        if !answerable {
            // No subscriber can answer this draft - either none is
            // attached or none renders one - so holding the caller
            // would park it forever. Drop the draft; the awaiting
            // caller sees the dropped receiver and fails closed.
            self.slack_drafts.lock().remove(&id);
            let (_ignored_sender, dead_receiver) = oneshot::channel();
            return (id, dead_receiver);
        }
        // **A second draft is a second ask.** The number is the ask's identity
        // on the wire, so a draft that never moves it is never announced.
        self.sink.note(caller, PendingInteractionKind::SlackDraft);
        (id, receiver)
    }

    /// Whether a draft with this id is still registered to `caller`, without
    /// removing it - the read the dispatch guard makes before an answer.
    pub fn slack_draft_waiting(&self, id: Uuid, caller: &SessionSlot) -> bool {
        self.slack_drafts.lock().get(&id).is_some_and(|held| &held.owner == caller)
    }

    /// Every draft currently held, as the session that asked and the draft.
    /// A snapshot: a caller that answers one races this read, which is what
    /// the answering call's own return reports. Test-only: the record read a
    /// view takes is [`AskSource::asks`].
    #[cfg(any(test, feature = "testing"))]
    pub fn held_drafts(&self) -> Vec<(SessionSlot, SlackDraft)> {
        self.slack_drafts
            .lock()
            .values()
            .map(|held| (held.owner.clone(), held.draft.clone()))
            .collect()
    }

    /// Hold a draft without announcing it or waiting on one, for a host's
    /// fixture: the ask is drawn and counted, and answering it is the
    /// test's own business.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_draft(&self, seat: &SessionSlot, draft: SlackDraft) {
        let (sender, _receiver) = oneshot::channel();
        self.slack_drafts
            .lock()
            .insert(draft.id, ParkedSlackDraft { owner: seat.clone(), draft, sender });
    }

    /// Remove every Slack subscription the worker `label` owns in
    /// `project`, from the active set and the store. Worker teardown calls
    /// this so a despawned worker cannot strand records - and so a delivery
    /// for it does not fall through to the lead.
    pub fn remove_slack_subscriptions_for_worker(&self, project: &str, label: &str) {
        let removed: Vec<SlackSubscription> = {
            let mut subs = self.slack_subs.lock();
            let mut removed = Vec::new();
            subs.retain(|sub| {
                let owned = sub.project == project && sub.team_role.as_deref() == Some(label);
                if owned {
                    removed.push(sub.clone());
                }
                !owned
            });
            removed
        };

        if removed.is_empty() {
            return;
        }
        let mut workspaces: HashSet<String> = HashSet::new();
        for sub in &removed {
            if let Err(error) = self.store.remove_subscription(sub.id) {
                tracing::warn!(
                    target: "forge_tools::slack",
                    %error,
                    "removing a persisted Slack subscription failed",
                );
            }
            let _ = workspaces.insert(sub.workspace.clone());
        }
        // The worker's subscriptions are all gone by now, so the
        // alive-set prune drops exactly its ownership, and the cursor
        // clear drops the cursors nothing covers any more.
        for sub in &removed {
            self.prune_slack_threads(&sub.workspace);
            self.clear_slack_cursors_after_removal(sub);
        }
        self.emit.subscriptions_changed(project);
    }

    /// Answer a held draft, returning whether one was waiting for THIS
    /// caller. That return is also how a caller asks whether a draft is
    /// still outstanding: it is true exactly once, for the entry that was
    /// there. A draft held for another session is refused rather than
    /// answered, and a caller that has gone away is not an error.
    ///
    /// The removal is the one point every removal a view could have parked
    /// on funnels through - an answer, or a dead waiter - so the stand-down
    /// is emitted here. The one other removal,
    /// `register_slack_draft`'s no-answerable-subscriber path, drops a draft
    /// the update may already have reached an observer with: a subscriber
    /// that could ANSWER is what makes a draft answerable at all, and an
    /// observer draws no dock to stand down.
    pub fn resolve_slack_draft(
        &self,
        id: Uuid,
        caller: &SessionSlot,
        resolution: DraftResolution,
    ) -> bool {
        let mut drafts = self.slack_drafts.lock();
        if !drafts.get(&id).is_some_and(|held| &held.owner == caller) {
            return false;
        }
        let Some(held) = drafts.remove(&id) else { return false };
        drop(drafts);
        let ending = match &resolution {
            DraftResolution::Answered(SlackDraftAnswer::Approve) => {
                SlackDraftEnding::Answered { approved: true }
            }
            DraftResolution::Answered(SlackDraftAnswer::Decline) => {
                SlackDraftEnding::Answered { approved: false }
            }
            DraftResolution::Answered(SlackDraftAnswer::SendBack { .. }) => {
                SlackDraftEnding::SentBack
            }
            DraftResolution::Abandoned => SlackDraftEnding::Abandoned,
        };
        // A dead waiter is the abandoned path, and it has nothing to receive.
        if let DraftResolution::Answered(answer) = resolution {
            let _ = held.sender.send(answer);
        }
        let _ = self.emit.emit(SlackUpdate::DraftResolved { key: held.owner, id, ending });
        true
    }

    /// Record the last `ts` delivered for a conversation in a workspace,
    /// through the store so it survives a restart.
    pub fn set_slack_watermark(&self, workspace: &str, conversation: &str, ts: &str) {
        if let Err(error) = self.store.set_watermark(workspace, conversation, ts) {
            tracing::warn!(
                target: "forge_tools::slack",
                %error,
                "writing a Slack watermark failed",
            );
        }
    }

    /// Record a message forge posted, so the sweep refuses it by identity
    /// rather than by author. forge posts as the user, so this is the only
    /// thing that tells its own words from the ones Ved typed.
    pub fn record_slack_own_post(&self, workspace: &str, conversation: &str, ts: &str) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|now| now.as_secs())
            .unwrap_or_default();
        if let Err(error) = self.store.record_own_post(workspace, conversation, ts, now) {
            tracing::warn!(
                target: "forge_tools::slack",
                workspace,
                conversation = %conversation,
                %error,
                "writing a Slack own-post ledger row failed",
            );
        }
    }

    /// Whether forge posted this message itself. A ledger that cannot be
    /// read answers `false`: a self-post delivered once costs what a lost
    /// ledger costs, while refusing would drop what the user wrote. The
    /// failure is worth a line once, not once per message, because a
    /// persistent fault would otherwise echo every post back to the agent.
    pub fn slack_own_post(&self, workspace: &str, conversation: &str, ts: &str) -> bool {
        match self.store.own_post(workspace, conversation, ts) {
            Ok(own) => {
                // A read that works again ends the failure, so a later
                // fault is a new line rather than silence.
                self.slack_ledger_failures
                    .lock()
                    .remove(&(workspace.to_owned(), conversation.to_owned()));
                own
            }
            Err(error) => {
                let first = self
                    .slack_ledger_failures
                    .lock()
                    .insert((workspace.to_owned(), conversation.to_owned()));
                if first {
                    tracing::warn!(
                        target: "forge_tools::slack",
                        event_name = "slack_own_post_ledger_unreadable",
                        workspace,
                        conversation = %conversation,
                        %error,
                        "reading the Slack own-post ledger failed; own posts are treated as not ours until it reads",
                    );
                } else {
                    tracing::debug!(
                        target: "forge_tools::slack",
                        workspace,
                        conversation = %conversation,
                        %error,
                        "reading the Slack own-post ledger failed again",
                    );
                }
                false
            }
        }
    }

    /// Mark one call's post in flight until the guard drops, so a sweep
    /// that runs mid-post skips the conversation instead of delivering a
    /// message whose ts is not ledgered yet. A guard covers ONE call and
    /// its ledger write: an operation of several calls takes one per call,
    /// or a slow but healthy Slack would retire the mark mid-operation.
    pub fn begin_slack_post(
        self: &Arc<Self>,
        workspace: &str,
        conversation: &str,
    ) -> SlackPostInFlight {
        let key = (workspace.to_owned(), conversation.to_owned());
        let id = NEXT_POST_STAMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.slack_inflight.lock().entry(key.clone()).or_default().push(SlackPostStamp {
            id,
            since: Instant::now(),
            warned: false,
        });
        SlackPostInFlight { slack: Arc::clone(self), key, id }
    }

    /// How long the oldest live post call in `conversation` has been in
    /// flight, or `None`. A stamp past the request timeout is expired
    /// rather than obeyed, so a stalled call cannot silence the
    /// conversation. A stamp crossing half the budget says so once, on the
    /// transition; both are forge's own problems, so both are warnings.
    pub fn slack_post_in_flight(&self, workspace: &str, conversation: &str) -> Option<Duration> {
        let mut inflight = self.slack_inflight.lock();
        let mut empty: Vec<(String, String)> = Vec::new();
        let mut stalled: Vec<(String, String, Duration)> = Vec::new();
        let mut oldest: Option<Duration> = None;
        for ((marked_workspace, marked_address), stamps) in inflight.iter_mut() {
            stamps.retain(|stamp| stamp.since.elapsed() <= REQUEST_TIMEOUT);
            if stamps.is_empty() {
                empty.push((marked_workspace.clone(), marked_address.clone()));
                continue;
            }
            if marked_workspace != workspace || marked_address != conversation {
                continue;
            }
            for stamp in stamps.iter_mut() {
                let age = stamp.since.elapsed();
                if age > REQUEST_TIMEOUT / 2 && !stamp.warned {
                    stamp.warned = true;
                    stalled.push((marked_workspace.clone(), marked_address.clone(), age));
                }
                if oldest.is_none_or(|current| age > current) {
                    oldest = Some(age);
                }
            }
        }
        for key in empty {
            inflight.remove(&key);
            tracing::warn!(
                target: "forge_tools::slack",
                event_name = "slack_post_mark_expired",
                workspace = %key.0,
                conversation = %key.1,
                "a post's in-flight mark outlived its request timeout; the conversation is swept again",
            );
        }
        drop(inflight);
        for (workspace, conversation, age) in stalled {
            tracing::warn!(
                target: "forge_tools::slack",
                event_name = "slack_post_in_flight_stalled",
                workspace = %workspace,
                conversation = %conversation,
                age_ms = age.as_millis(),
                "a post has been in flight for most of its request budget; the conversation is being skipped",
            );
        }
        oldest
    }

    /// Fill in the display name on this workspace's conversation
    /// subscriptions that have none. A durable record is written through
    /// the store; an ephemeral one is named in memory only. Only unnamed
    /// records are touched: a conversation renamed since it was
    /// subscribed keeps the stored name.
    pub fn name_slack_conversation(&self, workspace: &str, conversation: &str, name: &str) {
        let updated: Vec<SlackSubscription> = {
            let mut subs = self.slack_subs.lock();
            let mut updated = Vec::new();
            for sub in subs.iter_mut() {
                if sub.workspace != workspace {
                    continue;
                }
                if let SlackSubscriptionTarget::Conversation { id, name: stored @ None, .. } =
                    &mut sub.target
                    && id.as_str() == conversation
                {
                    *stored = Some(name.to_owned());
                    updated.push(sub.clone());
                }
            }
            updated
        };
        if updated.is_empty() {
            return;
        }
        // The name is what the section draws, and a watched conversation
        // starts unnamed and heals on its first message - so the heal is the
        // common way a set moves. Announced before the store work, which a
        // run with no store skips: the set a client draws is the one in
        // memory, and a project's records may heal in the same sweep.
        for project in projects_of(&updated) {
            self.emit.subscriptions_changed(&project);
        }
        if !self.store.available() {
            return;
        }
        // An ephemeral ad-hoc-worker subscription lives in memory only by
        // design, and inserting it here would promote it to durable.
        let durable: Vec<Uuid> = match self.store.list_subscriptions() {
            Ok(stored) => stored.into_iter().map(|sub| sub.id).collect(),
            Err(error) => {
                tracing::warn!(
                    target: "forge_tools::slack",
                    %error,
                    "reading Slack subscriptions to heal a name failed",
                );
                return;
            }
        };
        for sub in &updated {
            if !durable.contains(&sub.id) {
                continue;
            }
            if let Err(error) = self.store.insert_subscription(sub) {
                tracing::warn!(
                    target: "forge_tools::slack",
                    %error,
                    "writing a Slack conversation name failed",
                );
            }
        }
    }

    /// Whether the conversation (or the `__mentions__` stream) has a sweep
    /// cursor yet. The cursor belongs to the conversation and is shared by
    /// every owner watching it, so a second subscriber seeds only when
    /// this is false.
    pub fn slack_has_cursor(&self, workspace: &str, conversation: &str) -> bool {
        matches!(self.store.watermark(workspace, conversation), Ok(Some(_)))
    }

    /// Track a thread because a delivered message anchors it, owned by the
    /// matching subscription. A new row's cursor starts at `since` - the
    /// delivered message's own ts, not the parent's: the parent of a
    /// mention can be weeks old, and a cursor seeded from it would
    /// idle-drop the thread before its first reply walk. A row that
    /// exists keeps its cursor and gains the owner if it was not there.
    pub fn follow_slack_thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        owner: &SlackThreadOwner,
        since: &str,
    ) {
        if !self.store.available() {
            return;
        }
        let idle_days = self.slack_thread_idle_days(workspace);
        let mut created = false;
        // One read-modify-write under the host's guard: a concurrent prune
        // that read the row first must not write back over the owner this
        // adds.
        let mut update = |record: &mut Option<SlackThreadRecord>| {
            if let Some(record) = record {
                if !record.owners.contains(owner) {
                    record.owners.push(owner.clone());
                }
                return;
            }
            // A thread whose newest reply is already older than the idle
            // window would be dropped the moment the sweep listed it, and
            // the sweep anchors every parent it can see on every tick -
            // so writing the row would rewrite it forever for nothing.
            if thread_idle_days(since) >= idle_days {
                return;
            }
            created = true;
            *record = Some(SlackThreadRecord {
                cursor: since.to_owned(),
                edit_watermark: Some(since.to_owned()),
                owners: vec![owner.clone()],
                participating: false,
            });
        };
        if let Err(error) =
            self.store.update_thread(workspace, conversation, parent_ts, &mut update)
        {
            tracing::warn!(
                target: "forge_tools::slack",
                %error,
                "reading or writing a Slack thread failed; the thread stays unfollowed",
            );
            return;
        }
        if created {
            let key = (workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned());
            // The edits this row must judge start at its seed: anything older
            // was the previous row's, and the first walk is ts-only anyway.
            self.slack_thread_edit_watermarks.lock().insert(key.clone(), since.to_owned());
            self.slack_thread_first_walks.lock().insert(key);
        }
    }

    /// Drop a removed thread's in-memory clocks, so a thread re-followed
    /// under the same parent starts clean.
    pub fn forget_thread_clocks(&self, workspace: &str, conversation: &str, parent_ts: &str) {
        let key = (workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned());
        self.slack_thread_full_reads.lock().remove(&key);
        self.slack_thread_first_walks.lock().remove(&key);
        self.slack_thread_edit_watermarks.lock().remove(&key);
    }

    /// Record that the user is in a thread, so every later walk reads past the
    /// cursor rather than the whole thread.
    pub fn mark_slack_thread_participating(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) {
        if !self.store.available() {
            return;
        }
        let mut update = |record: &mut Option<SlackThreadRecord>| {
            if let Some(record) = record.as_mut() {
                record.participating = true;
            }
        };
        if let Err(error) =
            self.store.update_thread(workspace, conversation, parent_ts, &mut update)
        {
            tracing::warn!(
                target: "forge_tools::slack",
                %error,
                "writing a Slack thread failed",
            );
        }
    }

    /// The idle window this workspace drops a thread after, from its
    /// `[[slack]]` entry: the followed set is per workspace, so its window
    /// is too. A workspace whose entry is gone keeps the default.
    fn slack_thread_idle_days(&self, workspace: &str) -> u64 {
        self.config
            .read()
            .iter()
            .find(|config| config.workspace.trim() == workspace)
            .map_or(DEFAULT_THREAD_IDLE_DAYS, |config| config.thread_idle_days)
    }

    /// The age a delivered block reaches before its header says so, from its
    /// `[[slack]]` entry. A workspace whose entry is gone keeps the default.
    pub fn slack_stale_after(&self, workspace: &str) -> Duration {
        let hours = self
            .config
            .read()
            .iter()
            .find(|config| config.workspace.trim() == workspace)
            .map_or(DEFAULT_STALE_AFTER_HOURS, |config| config.stale_after_hours);
        Duration::from_secs(hours.saturating_mul(3_600))
    }

    /// The threads tracked for a conversation, pruned while listed: an
    /// owner with no remaining subscription in the workspace is dropped
    /// from the record, a row whose last owner goes is deleted, and a row
    /// idle past the workspace's window is dropped with a debug log.
    pub fn slack_followed_threads(
        &self,
        workspace: &str,
        conversation: &str,
    ) -> Vec<SlackFollowedThread> {
        if !self.store.available() {
            return Vec::new();
        }
        let rows = match self.store.threads_for(workspace, conversation) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    target: "forge_tools::slack",
                    %error,
                    "reading Slack threads failed",
                );
                return Vec::new();
            }
        };
        let alive_owners: HashSet<(String, Option<String>)> = self
            .slack_subs
            .lock()
            .iter()
            .filter(|sub| sub.workspace == workspace)
            .map(|sub| (sub.project.clone(), sub.team_role.clone()))
            .collect();
        let idle_days = self.slack_thread_idle_days(workspace);
        let mut out = Vec::new();
        for (parent_ts, _record) in rows {
            // Each row is re-read and re-pruned inside one guarded update:
            // the listing above is only the set of keys, so an owner a
            // concurrent delivery added between the listing and here is
            // judged on the row as it stands, never overwritten from a stale
            // copy.
            let mut kept = None;
            let mut expired = false;
            let mut idle = false;
            let mut update = |record: &mut Option<SlackThreadRecord>| {
                let empty;
                let stale;
                {
                    let Some(existing) = record.as_mut() else { return };
                    existing.owners.retain(|owner| {
                        alive_owners.contains(&(owner.project.clone(), owner.team_role.clone()))
                    });
                    empty = existing.owners.is_empty();
                    stale = !empty && thread_idle_days(&existing.cursor) >= idle_days;
                    if !empty && !stale {
                        kept = Some(SlackFollowedThread {
                            parent_ts: parent_ts.clone(),
                            owners: existing.owners.clone(),
                            participating: existing.participating,
                        });
                        return;
                    }
                }
                expired = empty;
                idle = stale;
                *record = None;
            };
            if let Err(error) =
                self.store.update_thread(workspace, conversation, &parent_ts, &mut update)
            {
                tracing::warn!(
                    target: "forge_tools::slack",
                    %error,
                    "pruning a Slack thread failed",
                );
                continue;
            }
            if expired || idle {
                self.forget_thread_clocks(workspace, conversation, &parent_ts);
                if idle {
                    tracing::debug!(
                        target: "forge_tools::slack",
                        workspace,
                        conversation,
                        parent = %parent_ts,
                        "dropping an idle Slack thread",
                    );
                }
                continue;
            }
            if let Some(row) = kept {
                out.push(row);
            }
        }
        out
    }

    /// A thread's reply cursor, or `None` when the thread has none yet.
    pub fn slack_thread_watermark(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Result<Option<String>, String> {
        self.store
            .thread(workspace, conversation, parent_ts)
            .map(|record| record.map(|record| record.cursor))
    }

    /// Advance a thread's reply cursor, through the store so it survives a
    /// restart.
    pub fn set_slack_thread_watermark(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        ts: &str,
    ) {
        if !self.store.available() {
            return;
        }
        let mut update = |record: &mut Option<SlackThreadRecord>| {
            if let Some(record) = record {
                ts.clone_into(&mut record.cursor);
                return;
            }
            *record = Some(SlackThreadRecord {
                cursor: ts.to_owned(),
                edit_watermark: None,
                owners: Vec::new(),
                participating: false,
            });
        };
        if let Err(error) =
            self.store.update_thread(workspace, conversation, parent_ts, &mut update)
        {
            tracing::warn!(
                target: "forge_tools::slack",
                %error,
                "reading or writing a Slack thread cursor failed",
            );
        }
    }

    /// A thread's newest judged edit stamp. The map is a cache; a miss reads
    /// the row, so a restart judges edits against what the last process
    /// delivered instead of re-offering the thread's whole edit history.
    pub fn slack_thread_edit_watermark(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Option<String> {
        let key = (workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned());
        if let Some(mark) = self.slack_thread_edit_watermarks.lock().get(&key).cloned() {
            return Some(mark);
        }
        if !self.store.available() {
            return None;
        }
        let mut record = match self.store.thread(workspace, conversation, parent_ts) {
            Ok(Some(record)) => record,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(
                    target: "forge_tools::slack",
                    event_name = "slack_thread_edit_stamp_read_failed",
                    workspace,
                    conversation,
                    parent = %parent_ts,
                    %error,
                    "reading a Slack thread failed; its edits are judged from the beginning",
                );
                return None;
            }
        };
        // A row written before the stamp was stored has none. Its cursor
        // approximates it, and both costs are bounded to the upgrade: an
        // edit that landed before it and sits below the cursor but was never
        // judged is dropped, and an already-handed correction above the
        // cursor is re-offered once.
        let mark = if let Some(mark) = record.edit_watermark.clone() {
            mark
        } else {
            let seed = record.cursor.clone();
            record.edit_watermark = Some(seed.clone());
            if let Err(error) = self.store.set_thread(workspace, conversation, parent_ts, &record) {
                tracing::warn!(
                    target: "forge_tools::slack",
                    event_name = "slack_thread_edit_stamp_seed_failed",
                    workspace,
                    conversation,
                    parent = %parent_ts,
                    %error,
                    "writing a legacy Slack thread's edit stamp failed",
                );
            }
            seed
        };
        self.slack_thread_edit_watermarks.lock().insert(key, mark.clone());
        Some(mark)
    }

    /// Advance a thread's judged edit stamp, through the store so a restart
    /// does not re-offer the correction it clears. The mark only moves
    /// forward: a caller that read a stale value must not roll a newer one
    /// back.
    pub fn set_slack_thread_edit_watermark(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        ts: &str,
    ) {
        let key = (workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned());
        if !self.store.available() {
            self.cache_slack_thread_edit_watermark(&key, ts);
            return;
        }
        // No row: there is nothing durable to stamp, and a row created later
        // seeds its own, so the mark is cached and no row is written.
        let mut cached: Option<String> = None;
        let mut update = |record: &mut Option<SlackThreadRecord>| {
            let Some(record) = record.as_mut() else {
                cached = Some(ts.to_owned());
                return;
            };
            match record.edit_watermark.as_deref() {
                // The mark only moves forward: a caller that read a stale
                // value must not roll a newer one back.
                Some(mark) if ts <= mark => cached = Some(mark.to_owned()),
                _ => {
                    record.edit_watermark = Some(ts.to_owned());
                    cached = Some(ts.to_owned());
                }
            }
        };
        if let Err(error) =
            self.store.update_thread(workspace, conversation, parent_ts, &mut update)
        {
            tracing::warn!(
                target: "forge_tools::slack",
                event_name = "slack_thread_edit_stamp_advance_failed",
                workspace,
                conversation,
                parent = %parent_ts,
                %error,
                "reading or writing a Slack thread failed; its edit stamp was not advanced",
            );
            return;
        }
        if let Some(mark) = cached {
            self.cache_slack_thread_edit_watermark(&key, &mark);
        }
    }

    /// Cache a thread's edit stamp, moving it forward only.
    fn cache_slack_thread_edit_watermark(&self, key: &(String, String, String), ts: &str) {
        let mut marks = self.slack_thread_edit_watermarks.lock();
        if marks.get(key).is_none_or(|mark| ts > mark.as_str()) {
            marks.insert(key.clone(), ts.to_owned());
        }
    }

    /// Watch `conversation` because a mention arrived in it, owned by
    /// whoever subscribes to mentions in that workspace - not the lead,
    /// and not whichever session happens to be active. `All` mode is
    /// deliberate: pulled into a conversation by a mention, the agent
    /// should see what is said next rather than only the next mention.
    /// The cursor starts at `since` (the mention that pulled us in), so
    /// the channel's history is not swept. Returns whether a record was
    /// added. A label that is empty or the conversation id itself (the
    /// search path's fallback) stores as no name.
    pub fn auto_subscribe_slack_conversation(
        &self,
        workspace: &str,
        conversation: &str,
        since: &str,
        name: Option<&str>,
    ) -> bool {
        let owner = {
            let subs = self.slack_subs.lock();
            let owner = subs.iter().find(|sub| {
                sub.workspace == workspace
                    && matches!(sub.target, SlackSubscriptionTarget::Mentions)
            });
            let Some(owner) = owner else { return false };
            let covered = subs.iter().any(|sub| {
                sub.workspace == workspace
                    && sub.project == owner.project
                    && sub.team_role == owner.team_role
                    && matches!(
                        &sub.target,
                        SlackSubscriptionTarget::Conversation { id, .. } if id == conversation
                    )
            });
            if covered {
                return false;
            }
            (owner.project.clone(), owner.team_role.clone())
        };

        // The cursor starts at the mention that pulled us in - but only when
        // the conversation has no cursor yet. The cursor is shared by
        // every owner watching the conversation, and another owner's
        // pending window must survive this auto-subscribe; the mention
        // itself was delivered through search, not this cursor.
        if !self.slack_has_cursor(workspace, conversation) {
            self.set_slack_watermark(workspace, conversation, since);
        }

        self.add_slack_subscription(
            SlackSubscription {
                id: Uuid::new_v4(),
                workspace: workspace.to_owned(),
                project: owner.0,
                team_role: owner.1,
                target: SlackSubscriptionTarget::Conversation {
                    id: conversation.to_owned(),
                    name: name
                        .filter(|label| !label.is_empty() && *label != conversation)
                        .map(str::to_owned),
                    mode: SlackWatchMode::All,
                },
                created_at: SystemTime::now(),
            },
            true,
        );
        true
    }

    /// Per-workspace pump liveness, for the Inspector's SLACK section.
    /// One entry per workspace a pump has reported on.
    pub fn slack_connected_workspaces(&self) -> BTreeMap<String, bool> {
        self.slack_connected.lock().clone()
    }

    /// The workspaces whose pump is running. Test-only: the Inspector draws
    /// liveness, not the handle.
    #[cfg(any(test, feature = "testing"))]
    pub fn pump_labels(&self) -> Vec<String> {
        self.slack_subsystem.lock().keys().cloned().collect()
    }

    /// Seed one workspace's authenticated user id, standing in for the boot
    /// `auth.test`. Test-only, so a sweep test needs no live token.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_user_id(&self, workspace: &str, user_id: &str) {
        self.slack_user_ids.lock().insert(workspace.to_owned(), user_id.to_owned());
    }

    /// Age every live post mark for one conversation past its request
    /// budget, standing in for a call that used all of it. Test-only: what
    /// the expiry means is the production read's.
    #[cfg(any(test, feature = "testing"))]
    pub fn expire_post_marks(&self, workspace: &str, conversation: &str) {
        let key = (workspace.to_owned(), conversation.to_owned());
        let mut inflight = self.slack_inflight.lock();
        let Some(stamps) = inflight.get_mut(&key) else { return };
        let stale = Instant::now()
            .checked_sub(REQUEST_TIMEOUT + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        for stamp in stamps {
            stamp.since = stale;
        }
    }

    /// Watch a conversation for this owner, handing back the record that
    /// already exists when there is one - a second record renders as a second
    /// Inspector row for one channel. A watch whose mode differs updates that
    /// record, since the caller asked for something it did not have and there
    /// is no other route to change a mode. The check and the write share one
    /// guard, so two subscribes racing cannot both create.
    pub fn watch_slack_conversation(
        &self,
        workspace: &str,
        project: &str,
        team_role: Option<&str>,
        conversation: &str,
        mode: SlackWatchMode,
        durable: bool,
    ) -> SlackSubscription {
        // The announcement is made after the guards drop: it reads both
        // connectors' sets, and a second lock on the one held here is a
        // deadlock rather than a warning.
        let (sub, moved) = {
            let mut subs = self.slack_subs.lock();
            if let Some(existing) = subs.iter_mut().find(|sub| {
                sub.workspace == workspace
                    && sub.project == project
                    && sub.team_role.as_deref() == team_role
                    && matches!(
                        &sub.target,
                        SlackSubscriptionTarget::Conversation { id, .. } if id == conversation
                    )
            }) {
                let mut moved = false;
                if let SlackSubscriptionTarget::Conversation { mode: stored, .. } =
                    &mut existing.target
                {
                    // A same-mode watch writes the record it already holds,
                    // so the section moved for neither.
                    moved = *stored != mode;
                    *stored = mode;
                }
                if durable && let Err(error) = self.store.insert_subscription(existing) {
                    tracing::warn!(
                        target: "forge_tools::slack",
                        %error,
                        "persisting a Slack subscription failed",
                    );
                }
                (existing.clone(), moved)
            } else {
                let sub = SlackSubscription {
                    id: Uuid::new_v4(),
                    workspace: workspace.to_owned(),
                    project: project.to_owned(),
                    team_role: team_role.map(str::to_owned),
                    target: SlackSubscriptionTarget::Conversation {
                        id: conversation.to_owned(),
                        name: None,
                        mode,
                    },
                    created_at: SystemTime::now(),
                };
                if durable && let Err(error) = self.store.insert_subscription(&sub) {
                    tracing::warn!(
                        target: "forge_tools::slack",
                        %error,
                        "persisting a Slack subscription failed",
                    );
                }
                subs.push(sub.clone());
                (sub, true)
            }
        };
        if moved {
            self.emit.subscriptions_changed(project);
        }
        sub
    }

    /// Every Slack subscription owned in `project`, whichever session
    /// owns it. Backs the pump, which needs the whole set.
    pub fn slack_subscriptions_for_project(&self, project: &str) -> Vec<SlackSubscription> {
        self.slack_subs.lock().iter().filter(|s| s.project == project).cloned().collect()
    }

    /// Remove the subscription `id` in `project` only when its owner
    /// matches `owner` (`None` = a lead subscription, `Some(label)` =
    /// that worker's), from both the active set and the store.
    /// Returns the row as it stood (`None` when nothing matched). Backs
    /// the owner-scoped `slack__unsubscribe` so a caller removes only
    /// what it subscribed.
    pub fn remove_slack_subscription_owned_by(
        &self,
        project: &str,
        id: Uuid,
        owner: Option<&str>,
    ) -> Option<SlackSubscription> {
        let removed_sub = {
            let subs = self.slack_subs.lock();
            subs.iter()
                .find(|s| s.id == id && s.project == project && s.team_role.as_deref() == owner)
                .cloned()
        };
        let removed_sub = removed_sub?;
        {
            let mut subs = self.slack_subs.lock();
            subs.retain(|s| s.id != id);
        }
        if let Err(error) = self.store.remove_subscription(id) {
            tracing::warn!(
                target: "forge_tools::slack",
                %error,
                "removing a persisted Slack subscription failed",
            );
        }
        self.prune_slack_threads(&removed_sub.workspace);
        self.clear_slack_cursors_after_removal(&removed_sub);
        self.emit.subscriptions_changed(project);
        Some(removed_sub)
    }

    /// Clear the sweep cursors the removed subscription was the last
    /// cover for. "Has cursor" must mean a live pending window: a cursor
    /// left behind by the last unsubscribe replays the whole gap to a
    /// later subscriber as fresh deliveries, and the mention cursor has
    /// no baseline arm at all, so its clearing is what makes the
    /// re-subscribe seed fresh.
    pub fn clear_slack_cursors_after_removal(&self, removed: &SlackSubscription) {
        if !self.store.available() {
            return;
        }
        let remaining = self.slack_subs.lock();
        let covered = |conversation: &str| {
            remaining.iter().any(|sub| {
                sub.workspace == removed.workspace
                    && match &sub.target {
                        SlackSubscriptionTarget::Conversation { id, .. } => id == conversation,
                        SlackSubscriptionTarget::Mentions => conversation == MENTION_CURSOR,
                        SlackSubscriptionTarget::DirectMessages => false,
                    }
            })
        };
        let drop_cursor = |conversation: &str| match self
            .store
            .remove_watermark(&removed.workspace, conversation)
        {
            Ok(true) => tracing::debug!(
                target: "forge_tools::slack",
                workspace = %removed.workspace,
                conversation,
                "cleared a sweep cursor with no watcher left",
            ),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                target: "forge_tools::slack",
                %error,
                "clearing a Slack sweep cursor failed",
            ),
        };
        match &removed.target {
            SlackSubscriptionTarget::Conversation { id, .. } => {
                if !covered(id) {
                    drop_cursor(id);
                }
            }
            SlackSubscriptionTarget::Mentions => {
                if !covered(MENTION_CURSOR) {
                    drop_cursor(MENTION_CURSOR);
                }
            }
            SlackSubscriptionTarget::DirectMessages => {
                // The class covered every DM cursor the sweep baselined;
                // the ones no remaining subscription names are stale now.
                let Ok(cursors) = self.store.watermark_conversations(&removed.workspace) else {
                    tracing::warn!(
                        target: "forge_tools::slack",
                        workspace = %removed.workspace,
                        "reading Slack cursors for cleanup failed",
                    );
                    return;
                };
                for conversation in cursors {
                    if !covered(&conversation) {
                        drop_cursor(&conversation);
                    }
                }
            }
        }
    }

    /// Prune every followed thread in `workspace` against the same
    /// predicate the sweep's listing uses: an owner survives only while
    /// it holds ANY remaining subscription in the workspace. Runs when a
    /// subscription is removed, so a thread whose conversation is no
    /// longer swept does not freeze until some sweep happens to list it -
    /// [`Self::slack_followed_threads`] re-prunes as a backstop. The
    /// surviving set decides because an owner key removed with one
    /// subscription may still hold others: a Mentions target anchors
    /// threads in every conversation its mentions touched.
    pub fn prune_slack_threads(&self, workspace: &str) {
        if !self.store.available() {
            return;
        }
        let Ok(rows) = self.store.threads_for_workspace(workspace) else {
            tracing::warn!(
                target: "forge_tools::slack",
                workspace,
                "reading Slack threads for pruning failed",
            );
            return;
        };
        let alive_owners: HashSet<(String, Option<String>)> = self
            .slack_subs
            .lock()
            .iter()
            .filter(|sub| sub.workspace == workspace)
            .map(|sub| (sub.project.clone(), sub.team_role.clone()))
            .collect();
        for (conversation, parent_ts, _record) in rows {
            // The row is re-read and re-pruned inside one guarded update, so
            // an owner a concurrent delivery added between the listing and
            // here survives: the filter runs on the row as it stands.
            let mut pruned = false;
            let mut update = |record: &mut Option<SlackThreadRecord>| {
                let Some(existing) = record.as_mut() else { return };
                let before = existing.owners.len();
                existing.owners.retain(|owner| {
                    alive_owners.contains(&(owner.project.clone(), owner.team_role.clone()))
                });
                pruned = existing.owners.len() != before;
                if existing.owners.is_empty() {
                    *record = None;
                }
            };
            if let Err(error) =
                self.store.update_thread(workspace, &conversation, &parent_ts, &mut update)
            {
                tracing::warn!(
                    target: "forge_tools::slack",
                    %error,
                    "pruning a Slack thread failed",
                );
                continue;
            }
            if pruned {
                self.forget_thread_clocks(workspace, &conversation, &parent_ts);
            }
        }
    }

    /// Prove each workspace's token once, logging the team it belongs to
    /// or the failure. Idempotent, and a no-op with no `[[slack]]` entry.
    pub fn start_slack_verification(self: &Arc<Self>) {
        if self.clients.is_empty() {
            return;
        }
        if self.slack_verification_started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(slack) = weak.upgrade() else { return };
            for (label, result) in slack.clients.verify_all().await {
                match result {
                    Ok(auth) => {
                        // Kept because the pump needs the id to recognise
                        // `<@U...>` mentions, and `auth.test` is the only
                        // call that reports it.
                        slack.slack_user_ids.lock().insert(label.clone(), auth.user_id.clone());
                        tracing::info!(
                            target: "forge_tools::slack",
                            workspace = %label,
                            team = %auth.team,
                            user = %auth.user,
                            "slack workspace verified",
                        );
                    }
                    Err(error) => tracing::warn!(
                        target: "forge_tools::slack",
                        workspace = %label,
                        %error,
                        "slack auth.test failed; the mention stream stays down until it succeeds",
                    ),
                }
            }
        });
    }

    /// Whether boot could not read the durable Slack subscriptions.
    pub fn slack_subscription_load_failed(&self) -> bool {
        self.slack_load_failed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Re-prove one workspace's token in the background, so a boot-time
    /// auth failure heals itself instead of leaving the mention stream
    /// down until restart. Rate-limited to one attempt a minute per
    /// workspace; the caller is the host's user-id miss.
    pub fn retry_slack_user_id(self: &Arc<Self>, label: &str) {
        const RETRY_INTERVAL: Duration = Duration::from_secs(60);
        {
            let mut retries = self.slack_user_id_retries.lock();
            if retries
                .get(label)
                .is_some_and(|last| last.elapsed().is_ok_and(|elapsed| elapsed < RETRY_INTERVAL))
            {
                return;
            }
            retries.insert(label.to_owned(), SystemTime::now());
        }
        let Some(api) = self.clients.client(label) else { return };
        let weak = Arc::downgrade(self);
        let label = label.to_owned();
        tokio::spawn(async move {
            let result = api.auth_test().await;
            let Some(slack) = weak.upgrade() else { return };
            match result {
                Ok(auth) => {
                    slack.slack_user_ids.lock().insert(label.clone(), auth.user_id);
                    tracing::info!(
                        target: "forge_tools::slack",
                        workspace = %label,
                        user = %auth.user,
                        "slack user id recovered on retry",
                    );
                }
                Err(error) => tracing::warn!(
                    target: "forge_tools::slack",
                    workspace = %label,
                    %error,
                    "slack user-id retry failed; the mention stream stays down",
                ),
            }
        });
    }

    /// Start one pump per configured workspace that has at least one
    /// subscription. Idempotent per workspace, and a no-op with no
    /// `[[slack]]` entry.
    pub fn start_slack_subsystem(self: &Arc<Self>) {
        if self.clients.is_empty() {
            return;
        }
        for label in self.clients.labels() {
            if !self.slack_subs.lock().iter().any(|sub| sub.workspace == label) {
                continue;
            }
            let mut guard = self.slack_subsystem.lock();
            if guard.contains_key(&label) {
                continue;
            }
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            guard.insert(label.clone(), shutdown_tx);
            drop(guard);

            let (poll_seconds, dm_poll_seconds) = {
                let config = self.config.read();
                let entry = config.iter().find(|config| config.workspace.trim() == label.as_str());
                // Clamped because the two clocks share one token's allowance: a
                // value below the floor is the overrun the key exists to prevent.
                (
                    entry.map_or(DEFAULT_POLL_SECONDS, |config| config.poll_seconds),
                    entry
                        .map_or(DEFAULT_DM_POLL_SECONDS, |config| config.dm_poll_seconds)
                        .max(MIN_DM_POLL_SECONDS),
                )
            };
            let host: Arc<dyn SlackHost> = Arc::new(SlackSubsystemHost::new(self));
            tokio::spawn(crate::slack::run_workspace_pump(
                host,
                label,
                poll_seconds,
                dm_poll_seconds,
                shutdown_rx,
            ));
        }
    }

    /// Stop the Slack pump for any workspace that no longer has a
    /// subscription, and mark it disconnected. Pumps are per workspace, so
    /// one workspace losing its subscriptions must not keep another's
    /// pump alive - nor leave its own running with nothing to watch.
    pub fn stop_slack_subsystem_if_idle(&self) {
        let idle_labels: Vec<String> = {
            let subs = self.slack_subs.lock();
            let watched: HashSet<&str> = subs.iter().map(|sub| sub.workspace.as_str()).collect();
            self.slack_subsystem
                .lock()
                .keys()
                .filter(|label| !watched.contains(label.as_str()))
                .cloned()
                .collect()
        };
        if idle_labels.is_empty() {
            return;
        }
        let handles: Vec<_> = {
            let mut handles = self.slack_subsystem.lock();
            idle_labels.iter().filter_map(|label| handles.remove(label)).collect()
        };
        for handle in handles {
            let _ = handle.send(());
        }
        let mut connected = self.slack_connected.lock();
        for label in &idle_labels {
            connected.insert(label.clone(), false);
        }
    }

    /// `(label, failure)` pairs whose failed author lookup has not been
    /// reported yet, recording this one. The once-per-user line the sweep
    /// relies on.
    pub fn first_author_failure(&self, workspace: &str, user: &str) -> bool {
        self.slack_author_failures.lock().insert((workspace.to_owned(), user.to_owned()))
    }

    /// The same once-per-user line for a held draft's tag lookup, kept apart
    /// from the sweep's so neither spends the other's.
    pub fn first_draft_user_failure(&self, workspace: &str, user: &str) -> bool {
        self.slack_draft_user_failures.lock().insert((workspace.to_owned(), user.to_owned()))
    }

    /// The once-per-thread line for a draft's failed parent read.
    pub fn first_thread_failure(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> bool {
        self.slack_thread_failures.lock().insert((
            workspace.to_owned(),
            conversation.to_owned(),
            parent_ts.to_owned(),
        ))
    }

    /// A ready API for the pump: the host's HTTP factory with this call's
    /// timeout, and the workspace's own token.
    fn pump_client(&self, workspace: &str, timeout: Duration) -> Result<Arc<dyn SlackApi>, String> {
        let token = {
            let config = self.config.read();
            config
                .iter()
                .find(|config| config.workspace.trim() == workspace)
                .ok_or_else(|| format!("no [[slack]] entry named '{workspace}'"))?
                .token
                .clone()
        };
        let http = (self.http)(timeout)?;
        Ok(Arc::new(SlackClient::new(http, token)))
    }
}

impl AskSource for Slack {
    fn asks(&self, seat: &SessionSlot) -> Vec<Ask> {
        self.slack_drafts
            .lock()
            .values()
            .filter(|held| &held.owner == seat)
            .map(|held| Ask {
                id: held.draft.id,
                kind: PendingInteractionKind::SlackDraft,
                payload: PendingAsk::SlackDraft(Box::new(held.draft.clone())),
            })
            .collect()
    }

    fn held(&self, seat: &SessionSlot) -> Vec<(PendingInteractionKind, usize)> {
        let count = self.slack_drafts.lock().values().filter(|held| &held.owner == seat).count();
        vec![(PendingInteractionKind::SlackDraft, count)]
    }

    fn kinds(&self) -> &'static [PendingInteractionKind] {
        const KINDS: &[PendingInteractionKind] = &[PendingInteractionKind::SlackDraft];
        KINDS
    }
}

/// One live post call in a conversation: its identity, when it started,
/// and whether the stall line has been said for it.
pub struct SlackPostStamp {
    pub id: u64,
    pub since: Instant,
    pub warned: bool,
}

/// Stamps are identified by a process-wide counter, so a guard's `Drop`
/// removes its own stamp even after its entry was expired and its key
/// taken again by a later post.
static NEXT_POST_STAMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Clears one call's in-flight stamp however it ends, a failed call or a
/// panic included. It removes only its own stamp, never a later call's.
pub struct SlackPostInFlight {
    slack: Arc<Slack>,
    key: (String, String),
    id: u64,
}

impl Drop for SlackPostInFlight {
    fn drop(&mut self) {
        let mut inflight = self.slack.slack_inflight.lock();
        if let Some(stamps) = inflight.get_mut(&self.key) {
            stamps.retain(|stamp| stamp.id != self.id);
            if stamps.is_empty() {
                inflight.remove(&self.key);
            }
        }
    }
}

/// The [`SlackHost`] one pump drives: a `Weak<Slack>` behind the port, so
/// the subsystem can drop while a pump runs and every port call then
/// degrades to a no-op.
pub struct SlackSubsystemHost(std::sync::Weak<Slack>);

impl SlackSubsystemHost {
    pub fn new(slack: &Arc<Slack>) -> Self {
        Self(Arc::downgrade(slack))
    }
}

impl SlackHost for SlackSubsystemHost {
    fn client(&self, workspace: &str, timeout: Duration) -> Result<Arc<dyn SlackApi>, String> {
        let slack = self.0.upgrade().ok_or("the slack subsystem is gone")?;
        slack.pump_client(workspace, timeout)
    }

    fn user_id(&self, workspace: &str) -> Option<String> {
        let slack = self.0.upgrade()?;
        let cached = slack.slack_user_ids.lock().get(workspace).cloned();
        if cached.is_some() {
            return cached;
        }
        // A boot-time auth failure would otherwise keep the mention
        // stream down until restart; the retry heals it in the
        // background.
        slack.retry_slack_user_id(workspace);
        None
    }

    fn user_name(&self, workspace: &str, user: &str) -> Option<String> {
        let slack = self.0.upgrade()?;
        slack.slack_user_names.lock().get(&(workspace.to_owned(), user.to_owned())).cloned()
    }

    fn set_user_name(&self, workspace: &str, user: &str, name: &str) {
        let Some(slack) = self.0.upgrade() else { return };
        slack
            .slack_user_names
            .lock()
            .insert((workspace.to_owned(), user.to_owned()), name.to_owned());
    }

    fn subscriptions(&self, workspace: &str) -> Vec<SlackSubscription> {
        let Some(slack) = self.0.upgrade() else { return Vec::new() };
        slack.slack_subs.lock().iter().filter(|sub| sub.workspace == workspace).cloned().collect()
    }

    fn watermark(&self, workspace: &str, conversation: &str) -> Result<Option<String>, String> {
        let slack = self.0.upgrade().ok_or("the slack subsystem is gone")?;
        slack.store.watermark(workspace, conversation)
    }

    fn set_watermark(&self, workspace: &str, conversation: &str, ts: &str) {
        let Some(slack) = self.0.upgrade() else { return };
        slack.set_slack_watermark(workspace, conversation, ts);
    }

    fn followed_threads(&self, workspace: &str, conversation: &str) -> Vec<SlackFollowedThread> {
        let Some(slack) = self.0.upgrade() else { return Vec::new() };
        slack.slack_followed_threads(workspace, conversation)
    }

    fn thread_watermark(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Result<Option<String>, String> {
        let slack = self.0.upgrade().ok_or("the slack subsystem is gone")?;
        slack.slack_thread_watermark(workspace, conversation, parent_ts)
    }

    fn set_thread_watermark(&self, workspace: &str, conversation: &str, parent_ts: &str, ts: &str) {
        let Some(slack) = self.0.upgrade() else { return };
        slack.set_slack_thread_watermark(workspace, conversation, parent_ts, ts);
    }

    fn mark_thread_participating(&self, workspace: &str, conversation: &str, parent_ts: &str) {
        let Some(slack) = self.0.upgrade() else { return };
        slack.mark_slack_thread_participating(workspace, conversation, parent_ts);
    }

    fn thread_full_read_at(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Option<Instant> {
        let slack = self.0.upgrade()?;
        slack
            .slack_thread_full_reads
            .lock()
            .get(&(workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned()))
            .copied()
    }

    fn mark_thread_full_read(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        at: Instant,
    ) {
        let Some(slack) = self.0.upgrade() else { return };
        slack
            .slack_thread_full_reads
            .lock()
            .insert((workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned()), at);
    }

    fn thread_first_walk(&self, workspace: &str, conversation: &str, parent_ts: &str) -> bool {
        let Some(slack) = self.0.upgrade() else { return false };
        slack.slack_thread_first_walks.lock().contains(&(
            workspace.to_owned(),
            conversation.to_owned(),
            parent_ts.to_owned(),
        ))
    }

    fn clear_thread_first_walk(&self, workspace: &str, conversation: &str, parent_ts: &str) {
        let Some(slack) = self.0.upgrade() else { return };
        slack.slack_thread_first_walks.lock().remove(&(
            workspace.to_owned(),
            conversation.to_owned(),
            parent_ts.to_owned(),
        ));
    }

    fn thread_edit_watermark(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Option<String> {
        let slack = self.0.upgrade()?;
        slack.slack_thread_edit_watermark(workspace, conversation, parent_ts)
    }

    fn set_thread_edit_watermark(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        ts: &str,
    ) {
        let Some(slack) = self.0.upgrade() else { return };
        slack.set_slack_thread_edit_watermark(workspace, conversation, parent_ts, ts);
    }

    fn first_author_failure(&self, workspace: &str, user: &str) -> bool {
        let Some(slack) = self.0.upgrade() else { return true };
        slack.first_author_failure(workspace, user)
    }

    fn first_draft_user_failure(&self, workspace: &str, user: &str) -> bool {
        let Some(slack) = self.0.upgrade() else { return true };
        slack.first_draft_user_failure(workspace, user)
    }

    fn follow_thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        owner: SlackThreadOwner,
        since: &str,
    ) {
        let Some(slack) = self.0.upgrade() else { return };
        slack.follow_slack_thread(workspace, conversation, parent_ts, &owner, since);
    }

    fn set_connected(&self, workspace: &str, connected: bool) {
        let Some(slack) = self.0.upgrade() else { return };
        slack.slack_connected.lock().insert(workspace.to_owned(), connected);
    }

    fn name_conversation(&self, workspace: &str, conversation: &str, name: &str) {
        let Some(slack) = self.0.upgrade() else { return };
        slack.name_slack_conversation(workspace, conversation, name);
    }

    fn auto_subscribe(&self, workspace: &str, message: &SlackMessage) -> bool {
        let Some(slack) = self.0.upgrade() else { return false };
        slack.auto_subscribe_slack_conversation(
            workspace,
            &message.conversation,
            &message.ts,
            Some(&message.conversation_label),
        )
    }

    fn own_post(&self, workspace: &str, conversation: &str, ts: &str) -> bool {
        self.0.upgrade().is_some_and(|slack| slack.slack_own_post(workspace, conversation, ts))
    }

    fn post_in_flight(&self, workspace: &str, conversation: &str) -> Option<Duration> {
        self.0.upgrade().and_then(|slack| slack.slack_post_in_flight(workspace, conversation))
    }

    fn deliver(&self, subscription: &SlackSubscription, messages: &[SlackMessage]) -> bool {
        let Some(slack) = self.0.upgrade() else { return false };
        // Direct, not via the command bus: the pump needs the delivery's
        // own outcome, and a bus round-trip reports only that the command
        // was accepted - a mid-delivery failure would read as success and
        // advance the cursor past an undelivered message.
        slack.emit.deliver(&subscription.project, subscription.team_role.as_deref(), messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering};

    use forge_primitives::slack::{
        DEFAULT_STALE_AFTER_HOURS, DEFAULT_THREAD_IDLE_DAYS, SlackDraftOperation,
        SlackSubscriptionTarget, SlackWatchMode,
    };

    fn cfg(workspace: &str, token: &str) -> SlackConfig {
        SlackConfig {
            workspace: workspace.to_owned(),
            token: token.to_owned(),
            poll_seconds: 30,
            dm_poll_seconds: 150,
            thread_idle_days: 14,
            stale_after_hours: 24,
        }
    }

    fn sub_for(project: &str, team_role: Option<&str>) -> SlackSubscription {
        SlackSubscription {
            id: Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: project.to_owned(),
            team_role: team_role.map(str::to_owned),
            target: SlackSubscriptionTarget::DirectMessages,
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn sub_for_conversation(project: &str, team_role: Option<&str>, id: &str) -> SlackSubscription {
        let mut sub = sub_for(project, team_role);
        sub.target = SlackSubscriptionTarget::Conversation {
            id: id.to_owned(),
            name: None,
            mode: SlackWatchMode::All,
        };
        sub
    }

    fn sub_mentions_for(project: &str, team_role: Option<&str>) -> SlackSubscription {
        let mut sub = sub_for(project, team_role);
        sub.target = SlackSubscriptionTarget::Mentions;
        sub
    }

    fn owner(project: &str, team_role: Option<&str>) -> SlackThreadOwner {
        SlackThreadOwner { project: project.to_owned(), team_role: team_role.map(str::to_owned) }
    }

    fn draft(workspace: &str, conversation: &str) -> SlackDraft {
        SlackDraft {
            id: Uuid::new_v4(),
            workspace: workspace.to_owned(),
            conversation: conversation.to_owned(),
            conversation_label: conversation.to_owned(),
            thread_ts: None,
            text: "hello".to_owned(),
            operation: SlackDraftOperation::Post,
            thread: None,
            tagged: Vec::new(),
        }
    }

    fn answered(approved: bool) -> DraftResolution {
        DraftResolution::Answered(if approved {
            SlackDraftAnswer::Approve
        } else {
            SlackDraftAnswer::Decline
        })
    }

    fn sent_back(note: &str) -> DraftResolution {
        DraftResolution::Answered(SlackDraftAnswer::SendBack { note: note.to_owned() })
    }

    /// A ts shaped like Slack's and set to now, because a followed thread
    /// starts at a parent that was just delivered - an epoch-era fixture
    /// would read as idle and be dropped.
    fn recent_ts() -> String {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("past the epoch");
        format!("{}.{:06}", now.as_secs(), now.subsec_micros())
    }

    fn message(conversation: &str, ts: &str) -> SlackMessage {
        SlackMessage {
            workspace: "acme".to_owned(),
            conversation: conversation.to_owned(),
            conversation_label: conversation.to_owned(),
            ts: ts.to_owned(),
            thread_ts: None,
            user: Some("U9".to_owned()),
            author: None,
            text: "hello".to_owned(),
            parent_user_id: None,
            latest_reply: None,
            edited: None,
            files: Vec::new(),
        }
    }

    fn caller() -> SessionSlot {
        SessionSlot::lead("TestOrg", "forge")
    }

    /// The durable store, in memory: the same shape as the redb one, so the
    /// subsystem's policy is what these tests exercise and a host's own
    /// store keeps the schema.
    #[derive(Default)]
    struct TestStore {
        inner: Mutex<TestStoreInner>,
        /// Widens the per-call read into a window a concurrent write can land
        /// in, so `concurrent_followers_keep_every_owner` can tell the
        /// guarded read-modify-write from the two-call pair it replaced.
        slow_read: AtomicBool,
    }

    #[derive(Default)]
    struct TestStoreInner {
        available: bool,
        subscriptions: Vec<SlackSubscription>,
        watermarks: BTreeSet<(String, String, String)>,
        threads: std::collections::BTreeMap<(String, String, String), SlackThreadRecord>,
        own_posts: BTreeSet<(String, String, String)>,
    }

    impl TestStore {
        fn present() -> Self {
            Self {
                inner: Mutex::new(TestStoreInner { available: true, ..Default::default() }),
                ..Default::default()
            }
        }

        fn absent() -> Self {
            Self::default()
        }

        fn subscriptions(&self) -> Vec<SlackSubscription> {
            self.inner.lock().subscriptions.clone()
        }

        fn watermark(&self, workspace: &str, conversation: &str) -> Option<String> {
            self.inner
                .lock()
                .watermarks
                .iter()
                .find(|(w, c, _)| w == workspace && c == conversation)
                .map(|(_, _, ts)| ts.clone())
        }

        fn thread(
            &self,
            workspace: &str,
            conversation: &str,
            parent_ts: &str,
        ) -> Option<SlackThreadRecord> {
            self.inner
                .lock()
                .threads
                .get(&(workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned()))
                .cloned()
        }
    }

    impl SlackStore for TestStore {
        fn available(&self) -> bool {
            self.inner.lock().available
        }

        fn insert_subscription(&self, subscription: &SlackSubscription) -> Result<(), String> {
            let mut inner = self.inner.lock();
            inner.subscriptions.retain(|sub| sub.id != subscription.id);
            inner.subscriptions.push(subscription.clone());
            Ok(())
        }

        fn list_subscriptions(&self) -> Result<Vec<SlackSubscription>, String> {
            Ok(self.inner.lock().subscriptions.clone())
        }

        fn remove_subscription(&self, id: Uuid) -> Result<bool, String> {
            let mut inner = self.inner.lock();
            let before = inner.subscriptions.len();
            inner.subscriptions.retain(|sub| sub.id != id);
            Ok(inner.subscriptions.len() != before)
        }

        fn watermark(&self, workspace: &str, conversation: &str) -> Result<Option<String>, String> {
            Ok(self.watermark(workspace, conversation))
        }

        fn set_watermark(
            &self,
            workspace: &str,
            conversation: &str,
            ts: &str,
        ) -> Result<(), String> {
            let mut inner = self.inner.lock();
            inner.watermarks.retain(|(w, c, _)| !(w == workspace && c == conversation));
            inner.watermarks.insert((workspace.to_owned(), conversation.to_owned(), ts.to_owned()));
            Ok(())
        }

        fn remove_watermark(&self, workspace: &str, conversation: &str) -> Result<bool, String> {
            let mut inner = self.inner.lock();
            let before = inner.watermarks.len();
            inner.watermarks.retain(|(w, c, _)| !(w == workspace && c == conversation));
            Ok(inner.watermarks.len() != before)
        }

        fn watermark_conversations(&self, workspace: &str) -> Result<Vec<String>, String> {
            Ok(self
                .inner
                .lock()
                .watermarks
                .iter()
                .filter(|(w, _, _)| w == workspace)
                .map(|(_, c, _)| c.clone())
                .collect())
        }

        fn thread(
            &self,
            workspace: &str,
            conversation: &str,
            parent_ts: &str,
        ) -> Result<Option<SlackThreadRecord>, String> {
            let row = self.thread(workspace, conversation, parent_ts);
            if self.slow_read.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(row)
        }

        fn set_thread(
            &self,
            workspace: &str,
            conversation: &str,
            parent_ts: &str,
            record: &SlackThreadRecord,
        ) -> Result<(), String> {
            self.inner.lock().threads.insert(
                (workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned()),
                record.clone(),
            );
            Ok(())
        }

        fn update_thread(
            &self,
            workspace: &str,
            conversation: &str,
            parent_ts: &str,
            update: &mut dyn FnMut(&mut Option<SlackThreadRecord>),
        ) -> Result<Option<SlackThreadRecord>, String> {
            let key = (workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned());
            let mut inner = self.inner.lock();
            let mut record = inner.threads.get(&key).cloned();
            update(&mut record);
            if let Some(record) = record {
                inner.threads.insert(key, record.clone());
                Ok(Some(record))
            } else {
                inner.threads.remove(&key);
                Ok(None)
            }
        }

        fn threads_for(
            &self,
            workspace: &str,
            conversation: &str,
        ) -> Result<Vec<(String, SlackThreadRecord)>, String> {
            Ok(self
                .inner
                .lock()
                .threads
                .iter()
                .filter(|((w, c, _), _)| w == workspace && c == conversation)
                .map(|((_, _, parent), record)| (parent.clone(), record.clone()))
                .collect())
        }

        fn threads_for_workspace(
            &self,
            workspace: &str,
        ) -> Result<Vec<(String, String, SlackThreadRecord)>, String> {
            Ok(self
                .inner
                .lock()
                .threads
                .iter()
                .filter(|((w, _, _), _)| w == workspace)
                .map(|((_, conversation, parent), record)| {
                    (conversation.clone(), parent.clone(), record.clone())
                })
                .collect())
        }

        fn remove_thread(
            &self,
            workspace: &str,
            conversation: &str,
            parent_ts: &str,
        ) -> Result<bool, String> {
            Ok(self
                .inner
                .lock()
                .threads
                .remove(&(workspace.to_owned(), conversation.to_owned(), parent_ts.to_owned()))
                .is_some())
        }

        fn own_post(&self, workspace: &str, conversation: &str, ts: &str) -> Result<bool, String> {
            Ok(self.inner.lock().own_posts.contains(&(
                workspace.to_owned(),
                conversation.to_owned(),
                ts.to_owned(),
            )))
        }

        fn record_own_post(
            &self,
            workspace: &str,
            conversation: &str,
            ts: &str,
            _now: u64,
        ) -> Result<(), String> {
            self.inner.lock().own_posts.insert((
                workspace.to_owned(),
                conversation.to_owned(),
                ts.to_owned(),
            ));
            Ok(())
        }
    }

    /// One delivery the emit seam was asked to make: the project, the
    /// owner, and the batch.
    type Delivered = (String, Option<String>, Vec<SlackMessage>);

    /// The frames and host actions, recorded rather than sent.
    #[derive(Default)]
    struct TestEmit {
        updates: Mutex<Vec<SlackUpdate>>,
        announced: Mutex<Vec<String>>,
        delivered: Mutex<Vec<Delivered>>,
        answerable: AtomicBool,
        delivery_ok: AtomicBool,
    }

    impl TestEmit {
        fn answering() -> Self {
            Self {
                answerable: AtomicBool::new(true),
                delivery_ok: AtomicBool::new(true),
                ..Default::default()
            }
        }

        fn take_updates(&self) -> Vec<SlackUpdate> {
            std::mem::take(&mut *self.updates.lock())
        }

        fn take_announced(&self) -> Vec<String> {
            std::mem::take(&mut *self.announced.lock())
        }
    }

    impl SlackEmit for TestEmit {
        fn emit(&self, update: SlackUpdate) -> bool {
            self.updates.lock().push(update);
            self.answerable.load(Ordering::SeqCst)
        }

        fn subscriptions_changed(&self, project: &str) {
            self.announced.lock().push(project.to_owned());
        }

        fn deliver(
            &self,
            project: &str,
            team_role: Option<&str>,
            messages: &[SlackMessage],
        ) -> bool {
            self.delivered.lock().push((
                project.to_owned(),
                team_role.map(str::to_owned),
                messages.to_vec(),
            ));
            self.delivery_ok.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct TestSink {
        noted: Mutex<Vec<(SessionSlot, PendingInteractionKind)>>,
    }

    impl AskSink for TestSink {
        fn note(&self, seat: &SessionSlot, kind: PendingInteractionKind) {
            self.noted.lock().push((seat.clone(), kind));
        }

        fn resolve(&self, _seat: &SessionSlot, _id: Uuid) {}
    }

    struct Harness {
        slack: Arc<Slack>,
        store: Arc<TestStore>,
        emit: Arc<TestEmit>,
        sink: Arc<TestSink>,
    }

    fn harness() -> Harness {
        harness_with(vec![cfg("acme", "xoxp-test")])
    }

    fn harness_with(config: Vec<SlackConfig>) -> Harness {
        let clients = Arc::new(
            SlackWorkspaces::from_config(&config, &reqwest::Client::new()).expect("clients"),
        );
        let store = Arc::new(TestStore::present());
        let emit = Arc::new(TestEmit::answering());
        let sink = Arc::new(TestSink::default());
        let http: Arc<PumpHttp> = Arc::new(|_timeout| Ok(reqwest::Client::new()));
        let slack = Arc::new(Slack::new(
            clients,
            config,
            Vec::new(),
            false,
            SlackSeams {
                store: Arc::clone(&store) as Arc<dyn SlackStore>,
                emit: Arc::clone(&emit) as Arc<dyn SlackEmit>,
                sink: Arc::clone(&sink) as Arc<dyn AskSink>,
                http,
            },
        ));
        Harness { slack, store, emit, sink }
    }

    /// A host over a subsystem with one C1 subscription owned by
    /// `project`/`team_role` already in place.
    fn host_with_c1_subscriber(
        project: &str,
        team_role: Option<&str>,
    ) -> (SlackSubsystemHost, Arc<Slack>, Arc<TestStore>) {
        let h = harness();
        h.slack.add_slack_subscription(sub_for_conversation(project, team_role, "C1"), true);
        (SlackSubsystemHost::new(&h.slack), Arc::clone(&h.slack), h.store)
    }

    /// A worker's draft must not surface in the lead's dock prompt: the
    /// approval is answered by whoever will read the reply, and the agent
    /// waiting on the oneshot is the one that asked.
    #[test]
    fn a_draft_is_addressed_to_the_session_that_asked() {
        let h = harness();
        let asked = SessionSlot::worker("TestOrg", "forge", "worker");
        let _held = h.slack.register_slack_draft(&asked, draft("acme", "C1"));

        let mut announced = Vec::new();
        for update in h.emit.take_updates() {
            if let SlackUpdate::PostPending { key, .. } = update {
                announced.push(key);
            }
        }
        assert_eq!(
            announced,
            vec![asked.clone()],
            "the draft is addressed to the worker that asked, never the lead",
        );
        assert_eq!(
            h.sink.noted.lock().as_slice(),
            [(asked, PendingInteractionKind::SlackDraft)],
            "the host is told one ask arrived",
        );
    }

    #[test]
    fn answering_a_draft_that_is_not_pending_is_refused() {
        let h = harness();
        assert!(!h.slack.resolve_slack_draft(Uuid::new_v4(), &caller(), answered(true)));
    }

    /// No UI to answer the draft: the registry must not hold it, and the
    /// awaiting caller sees a dead receiver - the fail-closed read.
    #[tokio::test]
    async fn a_draft_with_no_ui_to_answer_it_fails_closed() {
        let h = harness();
        h.emit.answerable.store(false, Ordering::SeqCst);
        let (_id, decision) = h.slack.register_slack_draft(&caller(), draft("acme", "C1"));
        assert!(
            h.slack.held_drafts().is_empty(),
            "an unanswerable draft is dropped, not held forever",
        );
        assert!(decision.await.is_err(), "a dead receiver reads as not-approved");
    }

    /// A draft only an observer can see must fail closed the way one with no
    /// subscriber does. The emit still records the frame - an observer draws
    /// the ask it cannot answer - so this is what catches pointing the guard
    /// at a plain send.
    #[tokio::test]
    async fn a_draft_with_only_an_observer_fails_closed() {
        let h = harness();
        h.emit.answerable.store(false, Ordering::SeqCst);
        let (_id, decision) = h.slack.register_slack_draft(&caller(), draft("acme", "C1"));

        assert!(
            h.slack.held_drafts().is_empty(),
            "a draft only an observer can see is dropped, not held forever",
        );
        assert!(
            matches!(h.emit.take_updates().as_slice(), [SlackUpdate::PostPending { .. }]),
            "the observer is still delivered the draft it cannot answer",
        );
        assert!(decision.await.is_err(), "the awaiting caller sees a dead receiver");
    }

    #[test]
    fn answering_a_draft_removes_it() {
        let h = harness();
        let (id, _decision) = h.slack.register_slack_draft(&caller(), draft("acme", "C1"));

        assert!(
            h.slack.resolve_slack_draft(id, &caller(), answered(true)),
            "the draft was waiting",
        );
        assert!(
            !h.slack.resolve_slack_draft(id, &caller(), answered(true)),
            "and is gone once answered",
        );
    }

    /// The draft leaving the registry is the one moment every view has to
    /// hear about: each view keeps its own copy of the draft, and no
    /// other update clears it. The ending rides it, because a view that
    /// did not answer has nothing else to say what happened.
    #[test]
    fn a_resolved_draft_stands_the_views_down() {
        let h = harness();
        let (id, _decision) = h.slack.register_slack_draft(&caller(), draft("acme", "C1"));
        let _ = h.emit.take_updates();

        assert!(
            h.slack.resolve_slack_draft(id, &caller(), answered(true)),
            "the draft was waiting",
        );

        match h.emit.take_updates().as_slice() {
            [SlackUpdate::DraftResolved { key, id: got, ending }] => {
                assert_eq!(key, &caller());
                assert_eq!(*got, id);
                assert!(
                    matches!(ending, SlackDraftEnding::Answered { approved: true }),
                    "the stand-down names the draft's own id and how it ended",
                );
            }
            other => panic!("expected one stand-down, got {other:?}", other = other.len()),
        }
    }

    /// A send-back is an answer the waiting call has to read: the note is
    /// what it revises from, so the registry hands it through whole and
    /// tells every view the draft went back rather than out.
    #[tokio::test]
    async fn a_sent_back_draft_hands_its_note_to_the_waiting_call() {
        let h = harness();
        let (id, decision) = h.slack.register_slack_draft(&caller(), draft("acme", "C1"));
        let _ = h.emit.take_updates();

        assert!(
            h.slack.resolve_slack_draft(id, &caller(), sent_back("shorten the second line")),
            "the draft was waiting",
        );

        assert_eq!(
            decision.await.expect("the held draft answers"),
            SlackDraftAnswer::SendBack { note: "shorten the second line".to_owned() },
            "the reader's words reach the blocked call whole",
        );
        match h.emit.take_updates().as_slice() {
            [SlackUpdate::DraftResolved { ending, .. }] => {
                assert!(
                    matches!(ending, SlackDraftEnding::SentBack),
                    "and a view that did not answer hears the draft was sent back",
                );
            }
            other => panic!("expected one stand-down, got {}", other.len()),
        }
    }

    /// An answer is only applied by the session the draft was addressed
    /// to; another session naming the id is refused, and the draft stays
    /// waiting for its owner.
    #[test]
    fn another_session_cannot_answer_a_draft_it_was_not_addressed() {
        let h = harness();
        let asker = SessionSlot::worker("TestOrg", "forge", "worker");
        let other = SessionSlot::lead("TestOrg", "forge");
        let (id, _decision) = h.slack.register_slack_draft(&asker, draft("acme", "C1"));

        assert!(
            !h.slack.resolve_slack_draft(id, &other, answered(true)),
            "an answer from another session must be refused",
        );
        assert!(
            h.slack.resolve_slack_draft(id, &asker, answered(true)),
            "and the draft is still waiting for the session that asked",
        );
    }

    /// The registry at the record read a client takes on reconnect: the
    /// held draft is drawn and counted for the seat that asked, and for no
    /// other.
    #[test]
    fn the_ask_source_reports_the_held_draft() {
        let h = harness();
        let seat = SessionSlot::worker("TestOrg", "forge", "worker");
        let draft = draft("acme", "C1");
        let id = draft.id;
        let (_held, _receiver) = h.slack.register_slack_draft(&seat, draft);

        let asks = AskSource::asks(h.slack.as_ref(), &seat);
        assert!(
            asks.iter().any(|ask| {
                ask.id == id
                    && matches!(&ask.payload, PendingAsk::SlackDraft(draft) if draft.id == id)
            }),
            "the record read carries the held draft",
        );
        assert_eq!(
            AskSource::held(h.slack.as_ref(), &seat),
            vec![(PendingInteractionKind::SlackDraft, 1)],
        );
        assert!(AskSource::asks(h.slack.as_ref(), &caller()).is_empty());
        assert_eq!(AskSource::kinds(h.slack.as_ref()), &[PendingInteractionKind::SlackDraft],);
    }

    /// A seeded draft is what a host's guard parks: drawn, counted, and
    /// answerable by the test rather than by a client.
    #[test]
    fn a_seeded_draft_is_counted_and_drawn() {
        let h = harness();
        let seat = SessionSlot::worker("TestOrg", "forge", "worker");
        h.slack.seed_draft(&seat, draft("acme", "C1"));
        assert_eq!(
            AskSource::held(h.slack.as_ref(), &seat),
            vec![(PendingInteractionKind::SlackDraft, 1)],
        );
        assert_eq!(AskSource::asks(h.slack.as_ref(), &seat).len(), 1);
    }

    /// The auto-subscription lands on whoever asked to be told about
    /// mentions - not the lead, and not whichever session happens to be
    /// active.
    #[test]
    fn the_auto_subscription_lands_on_the_mention_subscriptions_owner() {
        let h = harness();
        h.slack.add_slack_subscription(sub_mentions_for("forge", Some("tester")), true);

        h.slack.auto_subscribe_slack_conversation("acme", "C1", "200.1", Some("general"));

        let added = h.slack.slack_subscriptions_for_project("forge");
        assert!(
            added.iter().any(|sub| sub.team_role.as_deref() == Some("tester")
                && sub.target
                    == SlackSubscriptionTarget::Conversation {
                        id: "C1".to_owned(),
                        name: Some("general".to_owned()),
                        mode: SlackWatchMode::All,
                    }),
            "the conversation lands on the mention subscription's owner: {added:?}",
        );
    }

    /// The mention sweep's label falls back to the conversation id when
    /// Slack omits the name; storing it would render `#C1` and read as
    /// a channel that does not exist.
    #[test]
    fn an_auto_subscription_stores_no_name_when_the_label_is_not_one() {
        let h = harness();
        h.slack.add_slack_subscription(sub_mentions_for("forge", Some("tester")), true);

        assert!(h.slack.auto_subscribe_slack_conversation("acme", "C1", "200.1", Some("C1")));
        assert!(h.slack.auto_subscribe_slack_conversation("acme", "C2", "200.1", Some("")));
        assert!(h.slack.auto_subscribe_slack_conversation("acme", "C3", "200.1", None));

        let named = h
            .slack
            .slack_subscriptions_for_project("forge")
            .into_iter()
            .filter_map(|sub| match sub.target {
                SlackSubscriptionTarget::Conversation { name, .. } => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(named.len(), 3, "every record contributed a name slot: {named:?}");
        assert!(
            named.iter().all(Option::is_none),
            "no record invents a name from the id or an empty label: {named:?}",
        );
    }

    /// The delivered message's label is the only name source for a
    /// conversation a mention pulled us into but the user never joined:
    /// `users.conversations` is membership-scoped, so the sweep can never
    /// name that one. Drives the trait seam rather than the helper, so a
    /// `None` handed to it cannot pass unnoticed.
    #[test]
    fn the_auto_subscribe_seam_names_the_pulled_in_conversation() {
        let h = harness();
        h.slack.add_slack_subscription(sub_mentions_for("forge", Some("tester")), true);
        let host = SlackSubsystemHost::new(&h.slack);

        let message = SlackMessage {
            workspace: "acme".to_owned(),
            conversation: "C9".to_owned(),
            conversation_label: "granite-staging-alerts".to_owned(),
            ts: "200.1".to_owned(),
            thread_ts: None,
            user: Some("U9".to_owned()),
            author: None,
            text: "ping".to_owned(),
            parent_user_id: None,
            latest_reply: None,
            edited: None,
            files: Vec::new(),
        };
        assert!(host.auto_subscribe("acme", &message), "the mention subscribes the conversation");

        let named =
            h.slack.slack_subscriptions_for_project("forge").into_iter().find_map(|sub| match sub
                .target
            {
                SlackSubscriptionTarget::Conversation { id, name, .. } if id == "C9" => Some(name),
                _ => None,
            });
        assert_eq!(
            named,
            Some(Some("granite-staging-alerts".to_owned())),
            "the search hit's label names the conversation at creation",
        );
    }

    /// The guard the doc comment promises: a conversation renamed since it
    /// was subscribed keeps the stored name rather than picking up the
    /// directory's current one.
    #[test]
    fn naming_a_conversation_leaves_an_already_named_record_alone() {
        let h = harness();
        let mut sub = sub_for_conversation("forge", None, "C1");
        sub.target = SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            name: Some("stale-name".to_owned()),
            mode: SlackWatchMode::All,
        };
        h.slack.add_slack_subscription(sub, true);

        h.slack.name_slack_conversation("acme", "C1", "renamed");

        let name = h.store.subscriptions().into_iter().find_map(|sub| match &sub.target {
            SlackSubscriptionTarget::Conversation { id, name, .. } if id == "C1" => name.clone(),
            _ => None,
        });
        assert_eq!(
            name,
            Some("stale-name".to_owned()),
            "a record that already has a name is never rewritten",
        );
    }

    /// The same conversation id can be watched in two workspaces, so the
    /// heal is scoped to the workspace it was called for.
    #[test]
    fn naming_a_conversation_leaves_another_workspace_alone() {
        let h = harness();
        h.slack.add_slack_subscription(sub_for_conversation("forge", None, "C1"), true);
        let mut other = sub_for_conversation("forge", None, "C1");
        other.id = Uuid::new_v4();
        other.workspace = "beta".to_owned();
        h.slack.add_slack_subscription(other, true);

        h.slack.name_slack_conversation("acme", "C1", "ved-test");

        let named: BTreeMap<String, Option<String>> = h
            .store
            .subscriptions()
            .iter()
            .filter_map(|sub| match &sub.target {
                SlackSubscriptionTarget::Conversation { id, name, .. } if id == "C1" => {
                    Some((sub.workspace.clone(), name.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            named.get("acme"),
            Some(&Some("ved-test".to_owned())),
            "the named workspace's record is healed",
        );
        assert_eq!(named.get("beta"), Some(&None), "the other workspace's record is untouched");
    }

    /// An ephemeral ad-hoc-worker subscription is in memory only by design,
    /// so healing its name must not write it to the store and resurrect it
    /// on the next boot under a worker label that is gone.
    #[test]
    fn naming_an_ephemeral_conversation_does_not_persist_it() {
        let h = harness();
        h.slack.add_slack_subscription(sub_for_conversation("forge", Some("tester"), "C1"), false);

        h.slack.name_slack_conversation("acme", "C1", "ved-test");

        assert!(
            h.store.subscriptions().is_empty(),
            "an ephemeral record never reaches the store: {:?}",
            h.store.subscriptions(),
        );
        let named =
            h.slack.slack_subscriptions_for_project("forge").into_iter().find_map(|sub| match sub
                .target
            {
                SlackSubscriptionTarget::Conversation { id, name, .. } if id == "C1" => Some(name),
                _ => None,
            });
        assert_eq!(
            named,
            Some(Some("ved-test".to_owned())),
            "the in-memory record still gets its name",
        );
    }

    /// The auto-subscribe seeds only when no cursor exists: the cursor is
    /// shared by every owner watching the conversation, and another
    /// owner's pending window must survive the mention that pulled us in.
    #[test]
    fn an_auto_subscribe_never_resets_another_owners_cursor() {
        let h = harness();
        h.slack.add_slack_subscription(sub_mentions_for("forge", Some("tester")), true);
        h.slack.set_slack_watermark("acme", "C1", "100.0");

        assert!(
            h.slack.auto_subscribe_slack_conversation("acme", "C1", "200.1", None),
            "the conversation is watched by nobody yet, so a record is added",
        );
        assert_eq!(
            h.store.watermark("acme", "C1"),
            Some("100.0".to_owned()),
            "the auto-subscribe never moves another owner's cursor",
        );
    }

    /// A write moves the set a client draws, so the host hears the project
    /// it moved - and only the project a write actually touched.
    #[test]
    fn a_subscription_write_announces_its_project() {
        let h = harness();
        h.slack.add_slack_subscription(sub_for("forge", None), true);
        assert_eq!(h.emit.take_announced(), vec!["forge".to_owned()]);
    }

    #[test]
    fn a_subscription_is_scoped_to_its_project_and_owner() {
        let h = harness();
        h.slack.add_slack_subscription(sub_for("forge", None), true);
        h.slack.add_slack_subscription(sub_for("forge", Some("tester")), true);
        h.slack.add_slack_subscription(sub_for("other", None), true);

        let visible = h.slack.slack_subscriptions_for_project("forge");
        assert_eq!(visible.len(), 2, "another project's subscription must not leak in");
        assert!(visible.iter().any(|s| s.team_role.is_none()));
        assert!(visible.iter().any(|s| s.team_role.as_deref() == Some("tester")));
    }

    #[test]
    fn a_worker_cannot_remove_another_owners_subscription() {
        let h = harness();
        let lead = sub_for("forge", None);
        h.slack.add_slack_subscription(lead.clone(), true);

        assert!(
            h.slack.remove_slack_subscription_owned_by("forge", lead.id, Some("tester")).is_none(),
            "a worker must not remove the lead's subscription",
        );
        assert_eq!(
            h.slack.slack_subscriptions_for_project("forge").len(),
            1,
            "a refused removal removes nothing",
        );
        assert!(h.slack.remove_slack_subscription_owned_by("forge", lead.id, None).is_some());
        assert!(h.slack.slack_subscriptions_for_project("forge").is_empty());
    }

    #[test]
    fn duplicate_workspace_labels_are_refused() {
        let configs = vec![cfg("acme", "xoxp-test"), cfg("acme", "xoxp-other")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("two workspaces sharing a label cannot be addressed apart");
        assert!(err.contains("acme"), "the error has to name the shared label, got: {err}");
    }

    #[test]
    fn an_empty_token_is_refused() {
        let configs = vec![cfg("acme", "")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("a blank token would fail on every call");
        assert!(
            err.contains("acme") && err.contains("empty token"),
            "the error has to name the workspace and the missing credential, got: {err}",
        );
    }

    #[test]
    fn an_empty_workspace_label_is_refused() {
        let configs = vec![cfg("", "xoxp-test")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("a workspace with no label cannot be addressed at all");
        assert!(
            err.contains("empty workspace label"),
            "the error has to name the label, got: {err}",
        );
    }

    /// The idle window is the workspace's own. Both sides of it are
    /// exercised at a distance the 14-day default would get wrong in
    /// opposite directions.
    #[test]
    fn a_workspace_configured_idle_window_is_what_the_prune_uses() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("past the epoch")
            .as_secs();
        let days_ago = |days: u64| format!("{}.{:06}", now - days * 24 * 60 * 60, 0);

        for (window, kept) in [(30_u64, 1_usize), (7, 0)] {
            let h = harness_with(vec![SlackConfig {
                thread_idle_days: window,
                ..cfg("acme", "xoxp-test")
            }]);
            h.slack.add_slack_subscription(sub_for_conversation("forge", None, "C1"), true);
            let host = SlackSubsystemHost::new(&h.slack);
            host.follow_thread("acme", "C1", &days_ago(20), owner("forge", None), &days_ago(20));

            assert_eq!(
                host.followed_threads("acme", "C1").len(),
                kept,
                "a thread quiet for 20 days under a {window}-day window",
            );
        }
    }

    /// The stale threshold is the workspace's own, and a workspace with no
    /// entry keeps the default.
    #[test]
    fn a_workspace_configures_its_own_stale_age() {
        let h =
            harness_with(vec![SlackConfig { stale_after_hours: 6, ..cfg("acme", "xoxp-test") }]);

        assert_eq!(h.slack.slack_stale_after("acme"), Duration::from_secs(6 * 3_600));
        assert_eq!(
            h.slack.slack_stale_after("gone"),
            Duration::from_secs(DEFAULT_STALE_AFTER_HOURS * 3_600),
            "a workspace with no entry keeps the default",
        );
    }

    /// A thread a delivered message anchors is tracked from the caller's
    /// `since`, owned once per session even when both followed it.
    #[test]
    fn a_followed_thread_starts_at_the_callers_since_and_gains_its_owner_once() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", Some("tester"));
        let parent = recent_ts();

        host.follow_thread("acme", "C1", &parent, owner("forge", Some("tester")), &parent);
        host.follow_thread("acme", "C1", &parent, owner("forge", Some("tester")), &parent);

        let threads = host.followed_threads("acme", "C1");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].parent_ts, parent);
        assert_eq!(threads[0].owners, vec![owner("forge", Some("tester"))]);
        assert_eq!(
            host.thread_watermark("acme", "C1", &parent).expect("read"),
            Some(parent.clone()),
            "a new thread starts at the caller's since",
        );

        // A second session following the same thread joins the record
        // rather than resetting it.
        slack.add_slack_subscription(sub_for_conversation("other", None, "C1"), true);
        host.follow_thread("acme", "C1", &parent, owner("other", None), &recent_ts());
        let threads = host.followed_threads("acme", "C1");
        assert_eq!(threads[0].owners.len(), 2, "the second owner is added");
        assert_eq!(
            host.thread_watermark("acme", "C1", &parent).expect("read"),
            Some(parent),
            "joining a thread does not move its cursor",
        );
    }

    /// **A follower's read and write are one critical section.** Thirty-two
    /// sessions follow the same thread at once and every owner must survive.
    /// The store's `thread` read is widened, so the two-call pair the port
    /// replaced (read, then write) loses owners to the interleaving while one
    /// guarded update cannot.
    ///
    /// Mutant: restore `thread` + `set_thread` in `follow_slack_thread` and
    /// this reddens.
    #[test]
    fn concurrent_followers_keep_every_owner() {
        let h = harness();
        h.store.slow_read.store(true, Ordering::SeqCst);
        let parent = recent_ts();
        let since = recent_ts();
        std::thread::scope(|scope| {
            for index in 0..32 {
                let slack = Arc::clone(&h.slack);
                let parent = parent.clone();
                let since = since.clone();
                scope.spawn(move || {
                    slack.follow_slack_thread(
                        "acme",
                        "C1",
                        &parent,
                        &owner("forge", Some(&format!("w{index}"))),
                        &since,
                    );
                });
            }
        });
        h.store.slow_read.store(false, Ordering::SeqCst);

        let row = h.store.thread("acme", "C1", &parent).expect("the row was written");
        assert_eq!(row.owners.len(), 32, "every follower's owner survives the read-modify-write");
    }

    /// The pinned split: two owners on one thread, one subscription
    /// removed - the remaining owner still receives and the removed one
    /// stops, and the row goes when the last owner does.
    #[test]
    fn a_removed_subscription_prunes_its_owner_and_the_last_one_deletes_the_row() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", Some("a"));
        slack.add_slack_subscription(sub_for_conversation("forge", Some("b"), "C1"), true);
        let parent = recent_ts();
        host.follow_thread("acme", "C1", &parent, owner("forge", Some("a")), &parent);
        host.follow_thread("acme", "C1", &parent, owner("forge", Some("b")), &parent);
        // A stamp above the row's cursor seed, so the assertion below cannot
        // pass on a cursor-derived value.
        let mark = {
            let secs: u64 =
                parent.split('.').next().expect("a seconds prefix").parse().expect("numeric");
            format!("{}.000000", secs + 60)
        };
        host.set_thread_edit_watermark("acme", "C1", &parent, &mark);

        let b_id = slack
            .slack_subscriptions_for_project("forge")
            .into_iter()
            .find(|sub| sub.team_role.as_deref() == Some("b"))
            .expect("b's subscription")
            .id;
        assert!(slack.remove_slack_subscription_owned_by("forge", b_id, Some("b")).is_some());

        let threads = host.followed_threads("acme", "C1");
        assert_eq!(threads.len(), 1, "the thread keeps being swept");
        assert_eq!(
            threads[0].owners,
            vec![owner("forge", Some("a"))],
            "the removed owner is pruned, the remaining one keeps the thread",
        );
        // The prune rewrites the row; the edit stamp rides it, and the
        // cache is dropped so only the store can answer.
        slack.forget_thread_clocks("acme", "C1", &parent);
        assert_eq!(
            host.thread_edit_watermark("acme", "C1", &parent).as_deref(),
            Some(mark.as_str()),
            "the pruned row keeps its edit stamp, or the next restart re-offers it",
        );

        let a_id = slack
            .slack_subscriptions_for_project("forge")
            .into_iter()
            .find(|sub| sub.team_role.as_deref() == Some("a"))
            .expect("a's subscription")
            .id;
        assert!(slack.remove_slack_subscription_owned_by("forge", a_id, Some("a")).is_some());
        assert!(
            host.followed_threads("acme", "C1").is_empty(),
            "a thread whose last owner goes stops being tracked",
        );
        assert_eq!(
            host.thread_watermark("acme", "C1", &parent).expect("read"),
            None,
            "the row is deleted, not just filtered from the listing",
        );
    }

    /// A thread with no reply newer than its cursor for the drop age is
    /// deleted; one inside the window survives.
    #[test]
    fn a_thread_idle_past_fourteen_days_is_dropped() {
        let (host, _slack, _store) = host_with_c1_subscriber("forge", None);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("past the epoch")
            .as_secs();
        let days_ago = |days: u64| format!("{}.{:06}", now - days * 24 * 60 * 60, 0);

        host.follow_thread("acme", "C1", &days_ago(15), owner("forge", None), &days_ago(15));
        host.follow_thread("acme", "C1", &days_ago(14), owner("forge", None), &days_ago(14));
        host.follow_thread("acme", "C1", &days_ago(13), owner("forge", None), &days_ago(13));

        let threads = host.followed_threads("acme", "C1");
        assert_eq!(
            threads.len(),
            1,
            "the 15- and 14-day idle threads are dropped, the 13-day one kept",
        );
        assert_eq!(threads[0].parent_ts, days_ago(13), "and the survivor is the young one");
    }

    /// The documented fail-closed arm: a cursor that does not parse reads
    /// as ancient, so the row drops rather than living forever.
    #[test]
    fn an_unparseable_thread_cursor_reads_as_ancient() {
        assert!(
            thread_idle_days("not-a-ts") >= DEFAULT_THREAD_IDLE_DAYS,
            "an unparseable cursor must drop the row, not keep it: {}",
            thread_idle_days("not-a-ts"),
        );
        assert_eq!(thread_idle_days(&recent_ts()), 0, "a fresh cursor is not idle");
    }

    /// A mention dug up a weeks-old thread: the row's idle clock runs
    /// from the follow (the mention's ts), never from the parent, or the
    /// thread would be dropped before its first reply walk.
    #[test]
    fn a_mention_in_an_old_thread_is_not_idle_dropped_by_its_parents_age() {
        let (host, _slack, _store) = host_with_c1_subscriber("forge", None);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("past the epoch")
            .as_secs();
        let twenty_days_ago = format!("{}.{:06}", now - 20 * 24 * 60 * 60, 0);

        host.follow_thread("acme", "C1", &twenty_days_ago, owner("forge", None), &recent_ts());

        let threads = host.followed_threads("acme", "C1");
        assert_eq!(
            threads.len(),
            1,
            "a thread followed now must survive a weeks-old parent: {threads:?}",
        );
    }

    /// Pruning runs at REMOVAL time, not only inside the sweep's listing:
    /// a thread whose only owner is removed is gone from the store
    /// before any sweep happens to list it.
    #[test]
    fn removing_the_last_owners_subscription_deletes_the_thread_row_at_once() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", Some("tester"));
        let parent = recent_ts();
        host.follow_thread("acme", "C1", &parent, owner("forge", Some("tester")), &parent);

        let id = slack
            .slack_subscriptions_for_project("forge")
            .into_iter()
            .find(|sub| sub.team_role.as_deref() == Some("tester"))
            .expect("the tester's subscription")
            .id;
        assert!(slack.remove_slack_subscription_owned_by("forge", id, Some("tester")).is_some());

        assert_eq!(
            slack.slack_thread_watermark("acme", "C1", &parent).expect("read"),
            None,
            "the row is pruned at removal, never left for a sweep to find",
        );
    }

    /// Pruning keys on the SURVIVING subscription set, not on the removed
    /// owner identity: an owner whose C1 subscription is removed may
    /// still hold a Mentions subscription anchoring threads elsewhere.
    #[test]
    fn removing_one_subscription_keeps_the_owners_surviving_anchors() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", Some("tester"));
        slack.add_slack_subscription(sub_mentions_for("forge", Some("tester")), true);
        let parent = recent_ts();
        host.follow_thread("acme", "C2", &parent, owner("forge", Some("tester")), &parent);

        let c1_id = slack
            .slack_subscriptions_for_project("forge")
            .into_iter()
            .find(|sub| {
                sub.target
                    == SlackSubscriptionTarget::Conversation {
                        id: "C1".to_owned(),
                        name: None,
                        mode: SlackWatchMode::All,
                    }
            })
            .expect("the C1 subscription")
            .id;
        assert!(slack.remove_slack_subscription_owned_by("forge", c1_id, Some("tester")).is_some());

        assert_eq!(
            host.followed_threads("acme", "C2").len(),
            1,
            "the owner's surviving Mentions subscription keeps its thread row",
        );
    }

    /// Worker teardown removes that worker's subscriptions - and only
    /// its own - from the active set, and prunes its thread rows in the
    /// same pass, so a delivery for it cannot fall through to the lead.
    #[test]
    fn worker_teardown_removes_its_subscriptions_and_threads_only() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", Some("a"));
        slack.add_slack_subscription(sub_for_conversation("forge", Some("b"), "C1"), true);
        let parent = recent_ts();
        host.follow_thread("acme", "C1", &parent, owner("forge", Some("a")), &parent);
        host.follow_thread("acme", "C1", &parent, owner("forge", Some("b")), &parent);

        slack.remove_slack_subscriptions_for_worker("forge", "a");

        let left = slack.slack_subscriptions_for_project("forge");
        assert_eq!(left.len(), 1, "a's subscription is gone, b's remains");
        assert_eq!(left[0].team_role.as_deref(), Some("b"));
        let threads = host.followed_threads("acme", "C1");
        assert_eq!(
            threads[0].owners,
            vec![owner("forge", Some("b"))],
            "a's ownership is pruned, b keeps the thread",
        );
    }

    #[test]
    fn advancing_a_thread_cursor_keeps_its_owners() {
        let (host, _slack, _store) = host_with_c1_subscriber("forge", Some("tester"));
        let parent = recent_ts();
        host.follow_thread("acme", "C1", &parent, owner("forge", Some("tester")), &parent);
        let advanced = recent_ts();
        host.set_thread_watermark("acme", "C1", &parent, &advanced);
        assert_eq!(
            host.thread_watermark("acme", "C1", &parent).expect("read"),
            Some(advanced),
            "the cursor advances to the newest delivered reply",
        );
        assert_eq!(
            host.followed_threads("acme", "C1")[0].owners,
            vec![owner("forge", Some("tester"))],
            "and the owners ride along untouched",
        );
    }

    #[test]
    fn a_conversation_already_watched_is_not_auto_subscribed_again() {
        let h = harness();
        h.slack.add_slack_subscription(sub_mentions_for("forge", None), true);
        h.slack.add_slack_subscription(sub_for_conversation("forge", None, "C1"), true);

        assert!(
            !h.slack.auto_subscribe_slack_conversation("acme", "C1", "200.1", None),
            "a conversation that owner already watches adds nothing",
        );
        assert_eq!(
            h.slack.slack_subscriptions_for_project("forge").len(),
            2,
            "and no third record",
        );
    }

    #[test]
    fn a_redelivered_message_is_dropped_once() {
        let h = harness();
        let message = message("C1", "200.1");

        assert!(
            !h.slack.slack_delivery_seen("forge", None, &message),
            "the first delivery is unseen",
        );
        h.slack.slack_delivery_commit("forge", None, std::slice::from_ref(&message));
        assert!(
            h.slack.slack_delivery_seen("forge", None, &message),
            "the same ts in the same conversation to the same owner is a re-run",
        );
    }

    /// The delivery memory makes a sweep re-run idempotent, and an edit is
    /// not a re-run: a correction keeps the message's ts, so the edit ts
    /// is part of the identity or the correction is swallowed as already
    /// delivered.
    #[test]
    fn a_correction_is_a_new_delivery_while_a_re_run_is_not() {
        let h = harness();
        let message = message("C1", "200.1");
        h.slack.slack_delivery_commit("forge", None, std::slice::from_ref(&message));

        let corrected = SlackMessage {
            text: "deploy is RED".to_owned(),
            edited: Some("250.1".to_owned()),
            ..message
        };
        assert!(
            !h.slack.slack_delivery_seen("forge", None, &corrected),
            "an edit past the delivered version is a new delivery, not a duplicate",
        );
        h.slack.slack_delivery_commit("forge", None, std::slice::from_ref(&corrected));
        assert!(
            h.slack.slack_delivery_seen("forge", None, &corrected),
            "and the same edit re-run is the duplicate the memory is for",
        );
    }

    /// A lead and a worker in one project are two destinations: one
    /// delivery must never mark the other as already done.
    #[test]
    fn a_delivery_to_one_owner_never_dedupes_another_owners() {
        let h = harness();
        let message = message("C1", "200.1");

        h.slack.slack_delivery_commit("forge", None, std::slice::from_ref(&message));
        assert!(
            !h.slack.slack_delivery_seen("forge", Some("tester"), &message),
            "the worker's delivery is its own, never the lead's duplicate",
        );
    }

    /// The own-post ledger is read by the workspace and the conversation, so
    /// a harness that swapped either would leave every delivery test green.
    #[test]
    fn the_own_post_port_reads_the_ledger_by_workspace_and_conversation() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", None);
        slack.record_slack_own_post("acme", "C1", "1700000000.000100");

        assert!(host.own_post("acme", "C1", "1700000000.000100"), "the recorded row is read back");
        assert!(
            !host.own_post("acme", "C2", "1700000000.000100"),
            "another conversation's ledger does not answer for it",
        );
        assert!(
            !host.own_post("beta", "C1", "1700000000.000100"),
            "another workspace's ledger does not answer for it",
        );
    }

    /// A post is addressed by whichever name the caller had: a DM by user
    /// id, a channel by name. The sweep holds the conversation and asks
    /// under each of its names.
    #[test]
    fn an_in_flight_mark_is_per_call_and_per_guard() {
        let h = harness();
        let host = SlackSubsystemHost::new(&h.slack);

        // Two calls into one conversation at once: the first to finish
        // does not clear the other's stamp.
        let first = h.slack.begin_slack_post("acme", "C789");
        let second = h.slack.begin_slack_post("acme", "C789");
        assert!(host.post_in_flight("acme", "C789").is_some(), "both calls are in flight");
        assert!(
            host.post_in_flight("beta", "C789").is_none(),
            "and only in the workspace that took the mark",
        );
        assert!(host.post_in_flight("acme", "C790").is_none(), "and only for that conversation");
        drop(first);
        assert!(
            host.post_in_flight("acme", "C789").is_some(),
            "one call finishing does not clear an overlapping one",
        );
        drop(second);
        assert!(host.post_in_flight("acme", "C789").is_none(), "the last one clears the mark");
    }

    /// A mark past the request timeout is expired, not obeyed: a stalled
    /// call must not silence the conversation, and the expired row is
    /// dropped rather than re-checked forever.
    #[test]
    fn an_expired_in_flight_mark_is_not_obeyed() {
        let h = harness();
        let host = SlackSubsystemHost::new(&h.slack);
        let stale = Instant::now()
            .checked_sub(REQUEST_TIMEOUT)
            .and_then(|instant| instant.checked_sub(Duration::from_secs(1)))
            .expect("the clock is past the timeout");
        h.slack.slack_inflight.lock().insert(
            ("acme".to_owned(), "C789".to_owned()),
            vec![SlackPostStamp { id: 1, since: stale, warned: false }],
        );

        assert!(
            host.post_in_flight("acme", "C789").is_none(),
            "an expired mark is expired, never obeyed",
        );
        assert!(
            h.slack.slack_inflight.lock().is_empty(),
            "and it is dropped, not left to be re-checked",
        );
    }

    /// An old guard that outlived its stamp must not clear a newer call's
    /// mark: `Drop` removes only the stamp it took.
    #[test]
    fn an_expired_guards_drop_leaves_a_newer_mark_alone() {
        let h = harness();
        let host = SlackSubsystemHost::new(&h.slack);

        let old = h.slack.begin_slack_post("acme", "C789");
        let stale = Instant::now()
            .checked_sub(REQUEST_TIMEOUT)
            .and_then(|instant| instant.checked_sub(Duration::from_secs(1)))
            .expect("the clock is past the timeout");
        h.slack
            .slack_inflight
            .lock()
            .get_mut(&("acme".to_owned(), "C789".to_owned()))
            .expect("the old guard's entry")
            .first_mut()
            .expect("its stamp")
            .since = stale;
        assert!(
            host.post_in_flight("acme", "C789").is_none(),
            "the old stamp is expired and pruned",
        );

        let fresh = h.slack.begin_slack_post("acme", "C789");
        drop(old);
        assert!(
            host.post_in_flight("acme", "C789").is_some(),
            "the old guard's drop must not clear the newer call's mark",
        );
        drop(fresh);
        assert!(host.post_in_flight("acme", "C789").is_none(), "and the newer one clears it");
    }

    /// **The real host is what seeds the clocks**, and nothing else drives
    /// it: a new row is a first walk, its edit watermark starts at the seed,
    /// and the mark is cleared only by the walk that read it.
    #[test]
    fn a_new_thread_seeds_its_clocks_and_the_first_walk_mark() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", Some("tester"));
        // A seed inside the idle window, or the host refuses to write the row
        // at all.
        let since = format!(
            "{}.000000",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("the clock is past the epoch")
                .as_secs()
        );
        slack.follow_slack_thread("acme", "C1", "100.0", &owner("forge", Some("tester")), &since);

        assert!(host.thread_first_walk("acme", "C1", "100.0"), "a new row is a first walk");
        assert!(host.thread_first_walk("acme", "C1", "100.0"), "a peek does not consume the mark");
        assert_eq!(
            host.thread_edit_watermark("acme", "C1", "100.0").as_deref(),
            Some(since.as_str()),
            "the edit watermark starts at the row's seed",
        );
        host.clear_thread_first_walk("acme", "C1", "100.0");
        assert!(!host.thread_first_walk("acme", "C1", "100.0"), "and is cleared once read");

        host.mark_thread_full_read("acme", "C1", "100.0", Instant::now());
        assert!(
            host.thread_full_read_at("acme", "C1", "100.0").is_some(),
            "a whole-read attempt is stamped",
        );

        slack.follow_slack_thread("acme", "C1", "100.0", &owner("forge", None), &since);
        assert!(
            !host.thread_first_walk("acme", "C1", "100.0"),
            "following an existing row again does not re-arm the mark",
        );
    }

    /// **A pruned thread forgets its clocks**, so the maps do not leak a row
    /// that no longer exists and a re-follow starts clean.
    #[test]
    fn a_pruned_thread_forgets_its_clocks() {
        let h = harness();
        let slack = Arc::clone(&h.slack);
        let host = SlackSubsystemHost::new(&h.slack);
        let since = format!(
            "{}.000000",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("the clock is past the epoch")
                .as_secs()
        );
        slack.follow_slack_thread("acme", "C1", "100.0", &owner("forge", Some("gone")), &since);
        assert!(host.thread_first_walk("acme", "C1", "100.0"));
        assert!(host.thread_edit_watermark("acme", "C1", "100.0").is_some());

        slack.prune_slack_threads("acme");

        assert!(!host.thread_first_walk("acme", "C1", "100.0"), "the first-walk mark is gone");
        assert_eq!(
            host.thread_edit_watermark("acme", "C1", "100.0"),
            None,
            "and the edit watermark with it",
        );
        assert_eq!(host.thread_full_read_at("acme", "C1", "100.0"), None, "as is the stamp");
    }

    /// **A correction already delivered must not come back after a restart.**
    /// The thread's edit stamp lives on its durable row; a process-lifetime
    /// map holding it empties on restart, and the next whole read then judges
    /// every edited reply against nothing and hands them all over again.
    #[test]
    fn a_restart_does_not_re_offer_a_thread_edit_already_delivered() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", Some("tester"));
        let since = format!(
            "{}.000000",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("the clock is past the epoch")
                .as_secs()
        );
        slack.follow_slack_thread("acme", "C1", "100.0", &owner("forge", Some("tester")), &since);
        // The pump hands over a reply's correction and records its stamp; a
        // newer reply then moves the reply cursor past it.
        host.set_thread_edit_watermark("acme", "C1", "100.0", "990.0");
        host.set_thread_watermark("acme", "C1", "100.0", "995.0");

        // A restart keeps the store and drops every process-lifetime clock.
        slack.forget_thread_clocks("acme", "C1", "100.0");

        assert_eq!(
            host.thread_edit_watermark("acme", "C1", "100.0").as_deref(),
            Some("990.0"),
            "the delivered correction's stamp has to survive the restart, or the next whole \
             read re-offers it",
        );
    }

    /// **The stamp only moves forward.** A caller that read a stale value -
    /// a store read that failed, say - must not roll a newer stamp back.
    #[test]
    fn an_older_edit_stamp_does_not_roll_the_stored_one_back() {
        let (host, slack, _store) = host_with_c1_subscriber("forge", Some("tester"));
        let since = recent_ts();
        let secs: u64 =
            since.split('.').next().expect("a seconds prefix").parse().expect("numeric");
        let newer = format!("{}.000000", secs + 120);
        let older = format!("{}.000000", secs + 60);
        slack.follow_slack_thread("acme", "C1", "100.0", &owner("forge", Some("tester")), &since);
        host.set_thread_edit_watermark("acme", "C1", "100.0", &newer);
        host.set_thread_edit_watermark("acme", "C1", "100.0", &older);

        assert_eq!(
            host.thread_edit_watermark("acme", "C1", "100.0").as_deref(),
            Some(newer.as_str()),
            "the cache keeps the newer stamp",
        );
        slack.forget_thread_clocks("acme", "C1", "100.0");
        // With the cache empty the guard has only the row to compare
        // against: a stale caller arriving after a restart must not roll it
        // back.
        host.set_thread_edit_watermark("acme", "C1", "100.0", &older);
        assert_eq!(
            host.thread_edit_watermark("acme", "C1", "100.0").as_deref(),
            Some(newer.as_str()),
            "and the row keeps it, so a restart cannot roll it back",
        );
    }

    /// A cursor survives while ANY subscription still covers its
    /// conversation: one owner leaving must not reset the other's window.
    #[test]
    fn a_surviving_second_subscription_keeps_the_conversation_cursor() {
        let h = harness();
        h.slack.add_slack_subscription(sub_for_conversation("forge", None, "C1"), true);
        h.slack.add_slack_subscription(sub_for_conversation("forge", Some("tester"), "C1"), true);
        h.slack.set_slack_watermark("acme", "C1", "100.0");

        let lead_id = h
            .slack
            .slack_subscriptions_for_project("forge")
            .into_iter()
            .find(|sub| sub.team_role.is_none())
            .expect("the lead's subscription")
            .id;
        assert!(h.slack.remove_slack_subscription_owned_by("forge", lead_id, None).is_some());

        assert_eq!(
            h.store.watermark("acme", "C1"),
            Some("100.0".to_owned()),
            "the worker still watches C1, so its pending window survives",
        );
    }

    /// The mention cursor follows the same rule: with the last Mentions
    /// subscription gone it is cleared, because the mention sweep has no
    /// baseline arm and a stale cursor would replay the whole stream.
    #[test]
    fn removing_the_last_mentions_subscription_clears_the_mention_cursor() {
        let h = harness();
        h.slack.add_slack_subscription(sub_mentions_for("forge", None), true);
        h.slack.set_slack_watermark("acme", MENTION_CURSOR, "100.0");

        let id = h.slack.slack_subscriptions_for_project("forge")[0].id;
        assert!(h.slack.remove_slack_subscription_owned_by("forge", id, None).is_some());

        assert_eq!(
            h.store.watermark("acme", MENTION_CURSOR),
            None,
            "a stale mention cursor would replay the entire stream",
        );
    }

    /// A run whose store could not open keeps its in-memory set and writes
    /// nothing durable: the no-store path the availability gate preserves.
    #[test]
    fn a_run_with_no_store_keeps_its_in_memory_set() {
        let clients = Arc::new(
            SlackWorkspaces::from_config(&[cfg("acme", "xoxp-test")], &reqwest::Client::new())
                .expect("clients"),
        );
        let store = Arc::new(TestStore::absent());
        let emit = Arc::new(TestEmit::answering());
        let sink = Arc::new(TestSink::default());
        let http: Arc<PumpHttp> = Arc::new(|_timeout| Ok(reqwest::Client::new()));
        let slack = Arc::new(Slack::new(
            clients,
            vec![cfg("acme", "xoxp-test")],
            Vec::new(),
            false,
            SlackSeams { store, emit, sink, http },
        ));

        slack.add_slack_subscription(sub_for_conversation("forge", None, "C1"), true);

        assert_eq!(
            slack.slack_subscriptions_for_project("forge").len(),
            1,
            "the subscription is still in the set the pumps sweep",
        );
        assert!(!slack.slack_has_cursor("acme", "C1"), "no store means no cursor");
        assert!(slack.slack_followed_threads("acme", "C1").is_empty());
        assert!(!slack.slack_own_post("acme", "C1", "1.0"));
        assert_eq!(
            slack.slack_thread_edit_watermark("acme", "C1", "100.0"),
            None,
            "the no-store run judges every edit from the beginning",
        );
    }

    /// The pump's delivery goes through the emit seam with the
    /// subscription's own owner, and its failure is reported rather than
    /// swallowed - the cursor must not advance over an undelivered batch.
    #[test]
    fn the_pump_delivers_through_the_emit_seam() {
        let h = harness();
        let sub = sub_for_conversation("forge", Some("tester"), "C1");
        let host = SlackSubsystemHost::new(&h.slack);
        let messages = vec![message("C1", "200.1")];

        assert!(host.deliver(&sub, &messages), "a successful delivery reports success");
        assert_eq!(
            h.emit.delivered.lock().as_slice(),
            [("forge".to_owned(), Some("tester".to_owned()), messages.clone())],
            "the delivery names the subscription's project and owner",
        );

        h.emit.delivery_ok.store(false, Ordering::SeqCst);
        assert!(!host.deliver(&sub, &messages), "a refused delivery reports failure");
    }
}
