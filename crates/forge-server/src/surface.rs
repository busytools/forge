//! The surface a view uses: named read verbs by subject, returning values
//! that carry no terminal type, and one dispatch verb for what a view acts
//! with.
//!
//! [`ViewSurface::dispatch`] is a verb rather than an accessor, so a second
//! view is handed the commands it needs and not the whole core, and changes
//! arrive on [`ViewSurface::subscribe`].

pub mod accounts;
pub mod agents;
pub mod composer;
pub mod connectors;
pub mod dictate;
pub mod inspector;
pub mod plugins;
pub mod reviews;
pub mod roster;
pub mod session;
pub mod usage;
pub mod workers;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::transcript::ChatUnit;
use forge_primitives::SessionSlot;
use forge_workspace::{Command, DispatchError, Workspace};

pub use accounts::{AccountsView, GatewayView};
pub use agents::{AgentRow, Agents, PendingKind};
pub use dictate::DictateView;
// A view compares the values the surface hands it, so it needs their names
// too - re-exported here rather than reached for in the crates below, which
// a view does not name.
pub use forge_primitives::ConversationHistory;
pub use forge_workspace::env::cli_version::CliVersionInfo;
pub use forge_workspace::protocol::PendingAsk;
pub use forge_workspace::{DictateFailure, DictateModelState, DictateOutcome, LoadingState};
pub use roster::Roster;
pub use session::SessionState;
pub use workers::{WorkerRef, Workers};

/// A view's read handle on the core.
pub struct ViewSurface {
    workspace: Arc<Workspace>,
    /// The pool's last scan and when it was taken, so a reader asking twice
    /// inside the window does not walk it twice. A `Mutex` rather than a
    /// second cache type: one report, one window.
    usage_cache:
        std::sync::Mutex<Option<(std::time::Instant, forge_primitives::token_usage::UsageReport)>>,
}

/// The update the core emits, re-exported: it is the workspace-to-view
/// protocol already, so a view subscribes to it rather than to a second
/// vocabulary this crate would have to keep in step.
pub use forge_workspace::SessionUpdate;

/// The wire shape a turn that finished arrives as. A view marking a
/// completion reads the `Result` through this rather than deciding for
/// itself which results count.
pub fn is_success_result(is_error: bool, subtype: &str) -> bool {
    !is_error && subtype == "success"
}

impl ViewSurface {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace, usage_cache: std::sync::Mutex::new(None) }
    }

    /// Dispatch one command to the core, which is how a view acts rather
    /// than only reads.
    ///
    /// A verb rather than an accessor, on purpose: a second view is handed
    /// the writes it needs and not the whole core, so nothing here grows a
    /// way to reach the workspace itself.
    pub fn dispatch(&self, command: Command) -> Result<(), DispatchError> {
        self.workspace.dispatch(command)
    }

    /// What the seat at `slot` is held on, as the core kept it, so a view
    /// that attached after a prompt landed still draws what it offers.
    /// `None` when the seat is holding nothing.
    /// The last fatal error, or `None` when nothing has failed fatally.
    ///
    /// App-level rather than the seat's: it names a startup that could not
    /// happen. It is held because the update carries no state of its own,
    /// so a view that attached afterwards has no other way to learn of it.
    pub fn fatal_error(&self) -> Option<forge_primitives::error::AppError> {
        self.workspace.last_fatal_error()
    }

    /// The statuspage's last answer, or `None`.
    ///
    /// App-level: the answer is the same for every viewer, so the core
    /// probes it once and every view reads this rather than fetching its
    /// own. `None` covers both a healthy statuspage and one that could not
    /// be reached, which is the fetch's own contract.
    pub fn service_status(&self) -> Option<forge_primitives::cloud::service_status::ServiceIssue> {
        self.workspace.service_status()
    }

    pub fn pending_ask(&self, slot: &SessionSlot) -> Option<PendingAsk> {
        self.workspace.pending_ask(slot)
    }

    /// The conversation's units, as the server folds them.
    ///
    /// `conversation` hands over the wire messages and lets a view fold them;
    /// this hands over the FOLD, which is what the terminal draws. A second
    /// view folds for itself: how a run of tool calls groups is a drawing
    /// decision, and the one thing that stays here is where a turn begins -
    /// `transcript::render` reports that in message terms, which is what a
    /// page is cut on.
    pub fn folded_units(&self, slot: &SessionSlot, cwd_raw: &Path) -> Vec<ChatUnit> {
        crate::transcript::render_units(&self.conversation(slot, cwd_raw).messages)
    }

    /// The core's own update stream. Every caller gets a receiver of its
    /// own, so a second view attaches beside the first rather than stealing
    /// its events.
    ///
    /// **A caller on this verb is a frontend that renders AND answers
    /// prompts.** A permission, question or Slack-draft request is parked on
    /// a reply, and the paths that raise one resolve it `Cancelled` when no
    /// subscriber can answer - so a view that draws a dock while declaring
    /// itself an observer draws prompts that are already dead, and a turn
    /// fails rather than waiting for the person looking at it. Whoever
    /// subscribes here answers through `dispatch`, which is the same path
    /// the TUI uses.
    ///
    /// This was the mirror's role while the TUI was the only frontend and
    /// the only thing that answered. A second view that renders makes that
    /// reason stop holding.
    ///
    /// The first caller to attach is handed what was emitted before it as
    /// well, so a notice raised during boot is not lost; a caller attaching
    /// after one already has inherits no backlog.
    ///
    /// A consumer that reads the stream and renders no prompt takes
    /// [`Self::subscribe_mirror`] instead.
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<SessionUpdate> {
        self.workspace.subscribe()
    }

    /// The core's own update stream for a consumer that reads it WITHOUT
    /// answering, and keeps no backlog.
    ///
    /// **The caller this exists for is a process-wide fold.** It folds into
    /// state of its own and never replies, so the answering role would park
    /// a permission, question or Slack draft waiting on an answer nobody
    /// sends. And it attaches at boot, before the terminal does, so taking
    /// the backlog would take the boot notice from the view that renders
    /// prompts - the notice belongs to whichever subscription attaches
    /// first, and a fold is not the view that draws it.
    ///
    /// A view with a dock to answer from takes [`Self::subscribe`].
    pub fn subscribe_mirror(&self) -> tokio::sync::mpsc::UnboundedReceiver<SessionUpdate> {
        self.workspace.subscribe_mirror()
    }

    /// The core's stream for a client attaching beside the terminal,
    /// registered with the role that client declared.
    ///
    /// **Neither role takes the pre-attach backlog.** It goes to the first
    /// subscriber, and the view that renders the boot notice is the terminal:
    /// a client attaching beside it must not take that notice away, whether
    /// or not the client can answer. A client reads what it missed from the
    /// subject's snapshot instead.
    ///
    /// The role decides one thing: whether the core may park a prompt on this
    /// connection's reply. `answering` is the client's own statement that it
    /// can show and answer a prompt.
    pub fn subscribe_client(
        &self,
        answering: bool,
    ) -> tokio::sync::mpsc::UnboundedReceiver<SessionUpdate> {
        if answering {
            self.workspace.subscribe_answerer()
        } else {
            self.workspace.subscribe_mirror()
        }
    }

    /// The projects, their sessions, and the per-project lists the
    /// Projects pane renders.
    pub fn roster(&self) -> Roster {
        Roster::collect(Arc::clone(&self.workspace))
    }

    /// One session's operational state. `cwd_raw` is the caller's own
    /// cwd for the session, which a git worker's worktree overrides.
    pub fn session(&self, slot: &SessionSlot, cwd_raw: &Path) -> SessionState {
        SessionState::collect(&self.workspace, slot, cwd_raw)
    }

    /// The live workers, per project.
    pub fn workers(&self) -> Workers {
        Workers::collect(Arc::clone(&self.workspace))
    }

    /// The installed and npm-published `claude` CLI versions. The same
    /// answer for every viewer, so the core holds it and a change arrives
    /// as `SessionUpdate::CliVersionChanged`; `None` until the boot probe
    /// lands.
    pub fn cli_version(&self) -> Option<CliVersionInfo> {
        self.workspace.cli_version()
    }

    /// The conversation the session at `slot` holds, read from its own
    /// transcript on disk: the same read a resume performs, for a view
    /// that arrived after the session was already running.
    ///
    /// Subscribe first, then call this. The read is the baseline the stream
    /// is applied on top of, so a message may be in both: a view resolves
    /// that by dropping the stream's copy when the read already carries its
    /// id, and reading first instead would lose whatever arrived in between,
    /// which no later read brings back.
    ///
    /// The one class the two halves cannot reconcile by id is the prompts
    /// the workspace injects. A cron, a Gotify delivery, a Slack bundle or a
    /// peer comm reaches the stream as text alone (`CronPromptAppended`,
    /// `GotifyNotificationAppended`, `SlackMessageAppended`) and reaches this
    /// read as the envelope-wrapped row the CLI persisted, with no id shared
    /// between them, so a view holding both has to match on the envelope's
    /// source and body instead.
    ///
    /// This reads a whole transcript off disk on the calling thread, with no
    /// size cap: a real session costs milliseconds, and the largest
    /// transcript on this machine 2.5 s in a release build. A caller must
    /// offload it rather than run it in a handler, the way this project's
    /// other async work does with a spawned task on its own channel.
    ///
    /// `cwd_raw` is the session's own cwd, which a git worker's worktree
    /// overrides. An empty `cwd_raw` resolves to the slot's recorded path,
    /// because the read's no-directory arm walks every `projects/` directory
    /// for the first file named after the session: that pays the walk per
    /// call and takes whichever copy of the same session it meets first. A
    /// slot with no live session, or one whose cwd no record places, reads as
    /// an empty conversation, and records which of the two it was, so a view
    /// drawing nothing can say which of them it met rather than only that it
    /// has nothing to draw.
    pub fn conversation(&self, slot: &SessionSlot, cwd_raw: &Path) -> ConversationHistory {
        let Some(session_id) = self.workspace.running_session_id_for(slot) else {
            tracing::debug!(
                event_name = "conversation_no_pooled_session",
                org = slot.org(),
                project = slot.project(),
                label = slot.label(),
                "no session is pooled for this slot, so nothing here can find its \
                 transcript; drawing the conversation empty",
            );
            return ConversationHistory::default();
        };
        let cwd_raw = if cwd_raw.as_os_str().is_empty() {
            let Some(recorded) = self.workspace.cwd_for_session(slot) else {
                tracing::debug!(
                    event_name = "conversation_cwd_unplaced",
                    org = slot.org(),
                    project = slot.project(),
                    label = slot.label(),
                    session_id = %session_id,
                    "no record places this session's working directory, so its transcript \
                     cannot be looked up; drawing the conversation empty",
                );
                return ConversationHistory::default();
            };
            PathBuf::from(recorded)
        } else {
            cwd_raw.to_path_buf()
        };
        let cwd = self.workspace.git_scan_cwd_for_session(slot, &cwd_raw);
        forge_workspace::session_history(
            self.workspace.config_dir(),
            &session_id,
            &cwd.to_string_lossy(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use forge_primitives::SessionSlot;
    use forge_workspace::Workspace;

    use super::{SessionUpdate, ViewSurface};
    use crate::test_support::logged;

    /// The claude versions are facts about the world rather than about a
    /// viewer, so a view reads the core's answer instead of probing for
    /// itself: nothing probed yet reads as nothing.
    #[tokio::test]
    async fn the_surface_reads_the_claude_version_the_core_holds() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));

        assert!(surface.cli_version().is_none(), "nothing probed yet reads as nothing");

        workspace.seed_test_cli_version(Some("2.1.156"), Some("2.1.201"));
        let held = surface.cli_version().expect("the core's snapshot reaches the view");

        assert_eq!(
            held.installed.as_deref(),
            Some("2.1.156"),
            "the verb carries the installed version the core holds",
        );
        assert_eq!(held.latest.as_deref(), Some("2.1.201"), "and the published one beside it");
    }

    /// The mirror is the call site, not the fanout. Both verbs sit on one
    /// `UpdateFanout`, whose own test pins its two halves; what this pins
    /// is which half each one calls.
    ///
    /// **A view attaching through this verb renders AND answers prompts, and
    /// it takes the stream whole.** The two travel together on purpose: the
    /// verb that hands over the backlog is the verb that answers, because
    /// both belong to the frontend that draws a dock. A view pointed at the
    /// mirror instead draws prompts that are already dead - the workspace
    /// resolves a permission, question or Slack draft `Cancelled` when no
    /// subscriber can answer - and nothing in a build or a test would say so.
    ///
    /// This is the one place in the tree that tells the two apart: every
    /// other test answers an update that was emitted after it attached, and
    /// green either way.
    #[tokio::test]
    async fn a_surface_subscriber_answers_and_takes_the_stream_whole() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));

        // Emitted with nothing attached, so the fanout holds it for
        // whichever caller attaches first.
        workspace.emit_for_test(SessionUpdate::CatalogLoaded);

        let mut subscribed = surface.subscribe();
        assert!(
            matches!(subscribed.try_recv(), Ok(SessionUpdate::CatalogLoaded)),
            "a view attaching through this verb takes the stream whole, held notice included",
        );
    }

    /// The other half of the pair, and the reason both verbs exist: a fold
    /// attaching through the mirror takes no backlog, so the notice held
    /// before anyone attached reaches the view that renders prompts instead
    /// of being swallowed by whichever subscription happened to attach
    /// first - which at boot is the fold, not the terminal.
    ///
    /// The fold also does not answer, so putting it on the answering verb
    /// would park every prompt it draws waiting on a reply nobody sends.
    #[tokio::test]
    async fn a_surface_mirror_leaves_the_backlog_for_the_view_that_renders_prompts() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));

        workspace.emit_for_test(SessionUpdate::CatalogLoaded);

        let mut mirror = surface.subscribe_mirror();
        assert!(mirror.try_recv().is_err(), "a fold attaching through the mirror takes no backlog");

        let mut answering = workspace.subscribe();
        assert!(
            matches!(answering.try_recv(), Ok(SessionUpdate::CatalogLoaded)),
            "the held notice is still there for the subscriber that renders prompts",
        );
    }

    const SESSION_A: &str = "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45";
    const SESSION_B: &str = "6c2d3e4f-5061-4b72-8839-ad1e2f3a4b56";

    /// Write `turns` user rows as the transcript of `session_id`, under the
    /// project key `cwd` resolves to: where the CLI would have left them.
    fn seed_transcript(projects: &Path, session_id: &str, turns: usize) {
        use std::fmt::Write as _;
        let mut body = String::new();
        for i in 0..turns {
            let _ = writeln!(
                body,
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"turn {i}\"}}}}"
            );
        }
        std::fs::write(projects.join(format!("{session_id}.jsonl")), body).expect("write");
    }

    /// A worker's transcript is written under its worktree's project key,
    /// not the project root's, so the verb has to resolve the cwd the way
    /// the session verb beside it does: a caller handing it the project
    /// root still reads the worker's own conversation rather than an
    /// empty one.
    #[test]
    fn conversation_resolves_a_workers_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _updates) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project dir");
        workspace.seed_test_project("forge", &project_root.to_string_lossy());

        let worker = SessionSlot::worker("TestOrg", "forge", "probe-a");
        let worktree = project_root.join(".claude/worktrees/probe-a");
        std::fs::create_dir_all(&worktree).expect("worktree");
        workspace.seed_test_running_session_id(&worker, SESSION_A);
        let project = ViewSurface::new(Arc::clone(&workspace))
            .roster()
            .project_named("forge")
            .expect("seeded project")
            .key
            .clone();
        workspace.insert_live_worker(
            &project,
            forge_workspace::WorkerEntry {
                label: "probe-a".to_owned(),
                charter: "charter".to_owned(),
                slot: worker.clone(),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead("TestOrg", "forge"),
                needs_tag: false,
                is_git_repo_at_spawn: true,
                diagnostic: None,
                kick: None,
            },
        );

        let worktree_str = worktree.to_string_lossy().into_owned();
        let projects = dir.path().join("projects").join(
            forge_workspace::userdata::catalog::scan::project_key_for_directory(Some(
                &worktree_str,
            )),
        );
        std::fs::create_dir_all(&projects).expect("projects dir");
        seed_transcript(&projects, SESSION_A, 2);

        let read = ViewSurface::new(Arc::clone(&workspace)).conversation(&worker, &project_root);

        assert_eq!(read.messages.len(), 2, "the worker reads its worktree transcript");
    }

    /// An empty `cwd_raw` is a state the launchpad produces, and the read
    /// must not carry it on. The slot's own record places the read instead,
    /// so a caller that holds no path still gets the session's conversation.
    /// The fixture writes a real `forge.toml`: the project has to be one the
    /// workspace loaded, not one only the test overlay knows.
    #[test]
    fn conversation_places_an_empty_cwd_from_the_session_record() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let lead = SessionSlot::lead("TestOrg", "forge");
        workspace.seed_test_running_session_id(&lead, SESSION_A);

        // The fixture's one project lives at `/tmp`; the transcript lands
        // under its key inside the fixture's own config dir.
        let projects = workspace.config_dir().join("projects").join(
            forge_workspace::userdata::catalog::scan::project_key_for_directory(Some("/tmp")),
        );
        std::fs::create_dir_all(&projects).expect("projects dir");
        seed_transcript(&projects, SESSION_A, 2);

        let read = ViewSurface::new(Arc::clone(&workspace)).conversation(&lead, Path::new(""));

        assert_eq!(read.messages.len(), 2, "the record places the read the caller could not");
    }

    /// A slot whose cwd no record places reads as empty, and not as whatever
    /// a walk of every project happened to find: given no directory, the scan
    /// answers with the first `<session_id>.jsonl` on disk, which here is
    /// another project directory's copy of the same session.
    #[test]
    fn conversation_does_not_scan_every_project_for_an_empty_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _updates) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let stranger = dir.path().join("projects").join("some-other-project");
        std::fs::create_dir_all(&stranger).expect("projects dir");
        seed_transcript(&stranger, SESSION_A, 2);

        let unplaced = SessionSlot::lead("TestOrg", "not-a-project");
        workspace.seed_test_running_session_id(&unplaced, SESSION_A);

        let read = ViewSurface::new(Arc::clone(&workspace)).conversation(&unplaced, Path::new(""));

        assert!(read.messages.is_empty(), "no recorded cwd reads as empty, not as a stranger's");
    }

    /// The verb reads the slot's own transcript, and a slot with no
    /// occupant reads as empty: a view arriving before a session connects
    /// must render an empty conversation, not another session's.
    #[test]
    fn conversation_reads_the_slot_and_not_a_neighbour() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _updates) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).expect("project dir");
        let cwd_str = cwd.to_string_lossy().into_owned();
        let projects = dir.path().join("projects").join(
            forge_workspace::userdata::catalog::scan::project_key_for_directory(Some(&cwd_str)),
        );
        std::fs::create_dir_all(&projects).expect("projects dir");
        seed_transcript(&projects, SESSION_A, 2);
        seed_transcript(&projects, SESSION_B, 3);

        // One project, three seats: the neighbours differ by label rather
        // than by project, so a read that resolves the wrong seat is the
        // one this catches.
        let a = SessionSlot::lead("TestOrg", "forge");
        let b = SessionSlot::worker("TestOrg", "forge", "probe-a");
        let idle = SessionSlot::worker("TestOrg", "forge", "asleep");
        workspace.seed_test_running_session_id(&a, SESSION_A);
        workspace.seed_test_running_session_id(&b, SESSION_B);

        let surface = ViewSurface::new(Arc::clone(&workspace));
        let read_a = surface.conversation(&a, &cwd);
        let read_b = surface.conversation(&b, &cwd);

        assert_eq!(read_a.messages.len(), 2, "each slot reads its own transcript");
        assert_eq!(read_b.messages.len(), 3, "and the neighbour reads its own, not this one");
        let none = surface.conversation(&idle, &cwd);
        assert!(none.messages.is_empty(), "no occupant reads as empty");
    }

    /// A view drawing an empty conversation with nothing recorded gives a
    /// reader no way to tell a session with no transcript from a read that
    /// never happened, so each of the two silent paths says which it was,
    /// and names the slot it was silent for.
    #[test]
    fn an_empty_conversation_records_which_kind_of_empty_it_was() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _updates) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let surface = ViewSurface::new(Arc::clone(&workspace));

        // Whether one record carries `event_name`, at debug, naming all three
        // parts of `slot`: the org, project and label a reader acts on.
        let names = |events: &[(tracing::Level, String)], event_name: &str, slot: &SessionSlot| {
            events.iter().any(|(level, fields)| {
                *level == tracing::Level::DEBUG
                    && fields.contains(event_name)
                    && fields.contains(&format!(r#"org="{}""#, slot.org()))
                    && fields.contains(&format!(r#"project="{}""#, slot.project()))
                    && fields.contains(&format!(r#"label="{}""#, slot.label()))
            })
        };

        let idle = SessionSlot::lead("TestOrg", "forge");
        let no_session = logged(|| {
            let read = surface.conversation(&idle, Path::new("/tmp"));
            assert!(read.messages.is_empty(), "a slot with no occupant still reads as empty");
        });
        assert!(
            names(&no_session, "conversation_no_pooled_session", &idle),
            "the slot with no pooled session is named, and so is what was missing: \
             {no_session:?}",
        );

        let unplaced = SessionSlot::lead("TestOrg", "not-a-project");
        workspace.seed_test_running_session_id(&unplaced, SESSION_A);
        let no_cwd = logged(|| {
            let read = surface.conversation(&unplaced, Path::new(""));
            assert!(read.messages.is_empty(), "an unplaced cwd still reads as empty");
        });
        let read_as_the_other =
            no_cwd.iter().any(|(_, fields)| fields.contains("conversation_no_pooled_session"));
        assert!(
            names(&no_cwd, "conversation_cwd_unplaced", &unplaced)
                && no_cwd.iter().any(|(_, fields)| fields.contains(SESSION_A))
                && !read_as_the_other,
            "a cwd no record places is named as its own reason and not as the other one, \
             for the session it could not place: {no_cwd:?}",
        );
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use forge_workspace::Workspace;

    const CONFIG: &str = "\
[[orgs]]
name = \"TestOrg\"
accounts = [\"Granite\", \"OpenRouter-TM\"]

[[orgs.projects]]
name = \"forge\"
path = \"/tmp\"

[[accounts]]
display_name = \"Granite\"
token = \"t\"
models = [\"claude-sonnet-5\"]
provider = \"anthropic\"

[[accounts]]
display_name = \"OpenRouter-TM\"
token = \"t\"
models = [\"deepseek-v4.1-flash\"]
provider = \"openrouter\"
base_url = \"https://openrouter.ai/api\"

[dictate]
enabled = true
models_dir = \"/tmp/forge-dictate-models\"
";

    /// A workspace over a tempdir config with two accounts and one
    /// project, with its store open, plus the tempdir that must outlive
    /// it.
    pub(crate) fn workspace() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(forge.join("forge.toml"), CONFIG).expect("write forge.toml");
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("workspace");
        workspace.install_db_for_test(
            forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        (Arc::new(workspace), dir)
    }

    /// A workspace whose durable Gotify and Slack subscriptions were
    /// written before it opened its store, so the boot load puts them in
    /// memory and the reads see real rows.
    pub(crate) fn workspace_with_connector_subs() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(forge.join("forge.toml"), CONFIG).expect("write forge.toml");
        let app_support = dir.path().join("app-support");
        std::fs::create_dir_all(&app_support).expect("app-support/");
        {
            // `new_for_test` opens this exact path and loads both sets at
            // construction, so the rows have to be there first.
            let db =
                forge_workspace::store::Db::open(&app_support.join("db.redb")).expect("open db");
            forge_workspace::store::gotify::insert(&db, &gotify_sub("forge"))
                .expect("insert gotify sub");
            forge_workspace::store::slack::insert(&db, &slack_sub("forge"))
                .expect("insert slack sub");
        }
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("workspace");
        (Arc::new(workspace), dir)
    }

    pub(crate) fn gotify_sub(project: &str) -> forge_primitives::GotifySubscription {
        forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: project.to_owned(),
            team_role: None,
            applications: vec!["forge".to_owned()],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    pub(crate) fn slack_sub(project: &str) -> forge_primitives::slack::SlackSubscription {
        forge_primitives::slack::SlackSubscription {
            id: uuid::Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: project.to_owned(),
            team_role: None,
            target: forge_primitives::slack::SlackSubscriptionTarget::DirectMessages,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }
}
