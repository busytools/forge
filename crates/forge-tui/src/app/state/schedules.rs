//! The SCHEDULES + GOTIFY snapshots on `App`: entries built from a
//! `ScheduleWakeup` tool_use plus the durable forge-cron snapshot
//! refreshed from the workspace, pruned on the ~1s tick and scoped to
//! the active session's own project and team role.

use forge_sessions::surface::ViewSurface;

impl super::App {
    /// Active session's SCHEDULES entries (Inspector SCHEDULES
    /// section). Pruned by the ~1s timer tick.
    pub fn schedules(&self) -> &[crate::app::state::types::ScheduleEntry] {
        self.active_session().map_or(&[], |s| s.schedules.as_slice())
    }

    /// Mutable accessor for the active session's SCHEDULES list, or
    /// `None` when no session is focused.
    pub(crate) fn schedules_mut(
        &mut self,
    ) -> Option<&mut Vec<crate::app::state::types::ScheduleEntry>> {
        self.active_bucket_mut().map(|bucket| &mut bucket.schedules)
    }

    /// Insert/replace the session's single pending wakeup. The /loop
    /// dynamic-pacing mechanism re-arms each turn so at most one
    /// `Wakeup` entry survives: a new `ScheduleWakeup` replaces any
    /// prior one, whatever the tool call that carried it.
    pub fn upsert_wakeup_from_tool_input(&mut self, reason: &str, fire_at: std::time::SystemTime) {
        // #302 redux: wakeups are inherently session-scoped - the
        // /loop dynamic-pacing mechanism re-arms each turn, no
        // `durable` flag exists. The CLI kills every live wakeup at
        // session close, so any wakeup replayed during
        // `load_resume_history` is an orphan. Skip the push so
        // SCHEDULES doesn't surface phantom wakeups post-resume.
        // Live operation is untouched - the /loop re-arm path is
        // replay_in_progress=false. Mirrors #291's monitor pattern at
        // `set_monitor_status`.
        if self.replay_in_progress {
            return;
        }
        let now = std::time::SystemTime::now();
        let Some(schedules) = self.schedules_mut() else {
            return;
        };
        schedules.retain(|e| !matches!(e.kind, crate::app::state::types::ScheduleKind::Wakeup));
        schedules.push(crate::app::state::types::ScheduleEntry {
            kind: crate::app::state::types::ScheduleKind::Wakeup,
            label: if reason.is_empty() { "wakeup".to_owned() } else { reason.to_owned() },
            description: None,
            schedule: String::new(),
            fire_at: Some(fire_at),
            created_at: now,
        });
    }

    /// Drop a wakeup whose `fire_at` has passed. Called from the ~1s
    /// timer tick; the session's own entries are the only ones it walks.
    pub fn prune_expired_schedules(&mut self, now: std::time::SystemTime) {
        if self.active_session().is_none_or(|s| s.schedules.is_empty()) {
            return;
        }
        if let Some(schedules) = self.schedules_mut() {
            schedules.retain(|e| !e.is_expired(now));
        }
    }

    /// Recompute the active session's own durable forge-cron snapshot
    /// from the workspace, sorted soonest-first. Called on the ~1s ticker so
    /// the Inspector reads a cheap cached `Vec` instead of resolving the
    /// project + locking the workspace every render. Scopes by the active
    /// tab's stamped project NAME ([`Self::active_project_name`]): every
    /// bucket carries its project from the moment it is minted, so the
    /// per-tick read never re-derives it from a stale cwd.
    /// Then narrows to the session's own `team_role`, so a lead and its
    /// workers each see only what they can act on.
    /// Empty when no session is focused or the session created no cron.
    /// Also humanizes the crons into `forge_schedule_rows`
    /// here (resolving the local timezone once) so the render never pays
    /// that per frame.
    pub fn refresh_forge_crons(&mut self) {
        let own_role = self.active_session_team_role();
        let mut crons = match (self.active_project_name(), self.surface()) {
            (Some(name), Some(surface)) => surface.roster().crons_for_project(&name),
            _ => Vec::new(),
        };
        crons.retain(|c| c.team_role == own_role);
        crons.sort_by_key(|c| c.next_fire);
        // Resolve the local zone (an OS probe) only when there are crons
        // to humanize - most sessions have none.
        self.forge_schedule_rows = if crons.is_empty() {
            Vec::new()
        } else {
            let now = std::time::SystemTime::now();
            let tz = forge_workspace::env::timezone::system_timezone();
            crons
                .iter()
                .map(|c| crate::ui::inspector_pane::forge_cron_to_schedule_entry(c, now, tz))
                .collect()
        };
        self.forge_crons = crons;
    }

    /// Refresh the Gotify snapshot the Inspector GOTIFY section reads:
    /// the active session's own subscriptions (scoped by the active
    /// tab's stamped project NAME then by own `team_role`, like
    /// `refresh_forge_crons`) plus the stream connection status. Called
    /// on the ~1s ticker so the render reads cached fields instead of
    /// locking the workspace each frame.
    pub fn refresh_gotify(&mut self) {
        let own_role = self.active_session_team_role();
        let project = self.active_project_name();
        let Some(ws) = self.workspace.as_ref() else {
            self.gotify_subs = Vec::new();
            self.gotify_connected = false;
            return;
        };
        let gotify =
            ViewSurface::new(std::sync::Arc::clone(ws)).connectors(project.as_deref()).gotify;
        self.gotify_connected = gotify.connected;
        self.gotify_subs = gotify.subscriptions;
        self.gotify_subs.retain(|s| s.team_role == own_role);
    }

    /// Refresh the Slack snapshot the Inspector SLACK section reads: the
    /// active session's own subscriptions, scoped the same way
    /// [`Self::refresh_gotify`] scopes its set, plus per-workspace pump
    /// liveness. Called on the same ticker.
    pub fn refresh_slack(&mut self) {
        let own_role = self.active_session_team_role();
        let project = self.active_project_name();
        let Some(ws) = self.workspace.as_ref() else {
            self.slack_subs = Vec::new();
            self.slack_connected = std::collections::BTreeMap::new();
            self.slack_load_failed = false;
            return;
        };
        let slack =
            ViewSurface::new(std::sync::Arc::clone(ws)).connectors(project.as_deref()).slack;
        self.slack_connected = slack.connected_workspaces;
        self.slack_load_failed = slack.load_failed;
        self.slack_subs = slack.subscriptions;
        self.slack_subs.retain(|s| s.team_role == own_role);
    }

    /// The team role that owns the active session: `None` for a project
    /// lead, `Some(label)` for a worker. Scopes the SCHEDULES + GOTIFY
    /// snapshots to what this session created, matching what
    /// `cron__list` / `cron__delete` let it act on.
    ///
    /// Resolved from the live-worker registry, never the sessions
    /// catalog - workers are deliberately absent from the catalog, so a
    /// catalog read reports every worker as a lead.
    pub(crate) fn active_session_team_role(&self) -> Option<String> {
        let key = self.active_session_key.as_ref()?;
        self.surface()?.workers().lookup(key).map(|worker| worker.label)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::App;
    use crate::app::state::tests::make_test_app;
    use pretty_assertions::assert_eq;

    #[test]
    fn upsert_wakeup_replaces_prior_wakeup() {
        let mut app = App::test_default();
        let t0 = std::time::SystemTime::UNIX_EPOCH;
        app.upsert_wakeup_from_tool_input("first", t0 + std::time::Duration::from_secs(60));
        app.upsert_wakeup_from_tool_input("second", t0 + std::time::Duration::from_secs(120));
        let s = app.schedules();
        assert_eq!(s.len(), 1, "re-armed wakeup replaces the prior one");
        assert_eq!(s[0].label, "second");
    }

    #[test]
    fn prune_expired_schedules_drops_passed_wakeup() {
        let mut app = App::test_default();
        let t0 = std::time::SystemTime::UNIX_EPOCH;
        let fire = t0 + std::time::Duration::from_secs(60);
        app.upsert_wakeup_from_tool_input("poll", fire);
        app.prune_expired_schedules(t0); // before fire - kept
        assert_eq!(app.schedules().len(), 1);
        app.prune_expired_schedules(fire); // at fire - dropped
        assert!(app.schedules().is_empty());
    }

    /// FIX (4th attempt): the reported bug state is an active web-api
    /// tab where GIT / PROCESSES render and the projects pane + top bar
    /// highlight web-api, yet SCHEDULES is blank - because the
    /// per-bucket project STAMP does not resolve. The fix resolves the
    /// active project through the SAME `resolve_active_project_view` the
    /// pane + top bar use (a catalog match on the real session UUID), so
    /// SCHEDULES populates despite an unresolvable stamp AND a blanked
    /// cwd_raw. This isolates the primary chain link: neither the stamp
    /// nor the cwd resolves here, only the key/catalog resolver.
    #[test]
    fn refresh_forge_crons_resolves_via_pane_resolver_when_stamp_unresolvable() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/Users/me/Projects/web-api";
        ws.seed_test_project("web-api", path);
        // Mirror production: record_connected_session stamps the on-disk
        // catalog at Connect, which is exactly what resolve_active_project_view
        // reads to highlight the active project in the pane + top bar.
        let uuid = "acbd8a76-448b-4dda-bb01-dd930cdd261a";
        ws.record_connected_session(path, uuid, None);
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "web-api".to_owned(),
            kind: CronKind::Recurring("18 9 * * 1-5".to_owned()),
            prompt: "market open".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        // Active tab is the real web-api session, but the stamp is empty
        // AND cwd_raw is blank - only the catalog resolver can succeed.
        // The slot carries the project's `(org, name)`, which is what
        // the pane/top-bar resolver matches on.
        let project =
            ws.list_projects().into_iter().find(|p| p.name == "web-api").expect("seeded project");
        let key = forge_workspace::SessionSlot::lead(project.org.clone(), project.name.clone());
        let mut bucket = crate::app::session::UiSession::new(key.clone(), "");
        bucket.cwd_raw = String::new();
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(
            app.active_project_name().as_deref(),
            Some("web-api"),
            "resolves via the pane/top-bar resolver despite an empty stamp + blank cwd",
        );
        assert_eq!(app.forge_crons, vec![cron], "SCHEDULES populates via the robust chain");
    }

    #[test]
    fn refresh_forge_crons_caches_humanized_schedule_rows() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/Users/me/Projects/web-api";
        ws.seed_test_project("web-api", path);
        let uuid = "acbd8a76-448b-4dda-bb01-dd930cdd261a";
        ws.record_connected_session(path, uuid, None);
        ws.seed_test_cron(CronEntry {
            id: CronId::from("c1"),
            project_name: "web-api".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "market open".to_owned(),
            description: Some("Morning digest".to_owned()),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        });
        let key = forge_workspace::SessionSlot::from_str_for_test(uuid);
        let bucket = crate::app::session::UiSession::new(key.clone(), "web-api");
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(app.forge_crons.len(), 1, "raw snapshot still populated");
        assert_eq!(
            app.forge_schedule_rows.len(),
            1,
            "the tick humanizes the cron into a cached presentation row",
        );
        let row = &app.forge_schedule_rows[0];
        assert_eq!(row.schedule, "daily at 09:00", "schedule humanized once on the tick");
        assert_eq!(row.description.as_deref(), Some("Morning digest"), "description headlines");
        assert_eq!(
            row.fire_at,
            Some(std::time::SystemTime::UNIX_EPOCH),
            "fire_at drives the countdown"
        );
    }

    /// Chain link: when the catalog has no entry for the active UUID
    /// (resolve_active_project_view misses), the active project still
    /// resolves from the bucket's stamp - which is minted from the
    /// cwd at Connect, so the read-time chain never re-derives it.
    #[test]
    fn refresh_forge_crons_resolves_via_stamp_when_no_catalog_entry() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/Users/me/Projects/web-api";
        ws.seed_test_project("web-api", path);
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "web-api".to_owned(),
            kind: CronKind::Recurring("18 9 * * 1-5".to_owned()),
            prompt: "market open".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        // Real UUID NOT in the catalog: only the bucket's stamp resolves.
        let key = forge_workspace::SessionSlot::from_str_for_test("uncatalogued-uuid");
        let mut bucket = crate::app::session::UiSession::new(key.clone(), "web-api");
        bucket.cwd_raw = path.to_owned();
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(app.active_project_name().as_deref(), Some("web-api"));
        assert_eq!(app.forge_crons, vec![cron], "SCHEDULES resolves via the bucket's stamp");
    }

    /// REPRODUCE (recurring SCHEDULES-blank bug, 3rd attempt): the
    /// Inspector scopes forge crons by the tab's stamped project NAME,
    /// never by re-deriving the project from `cwd_raw`. A bucket whose
    /// `project` is set but whose `cwd_raw` does NOT path-prefix-match
    /// the project's stored (expanded) path still surfaces the project's
    /// crons. The pre-fix cwd-prefix match returned empty for exactly
    /// this mismatch (here a tilde form vs the expanded project path) -
    /// the class of failure that kept web-api's SCHEDULES blank.
    #[test]
    fn refresh_forge_crons_scopes_by_stamped_project_name_not_cwd() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");

        // Project path is stored expanded; the bucket cwd is a tilde
        // form that cannot prefix-match it.
        ws.seed_test_project("web-api", "/Users/me/Projects/web-api");
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "web-api".to_owned(),
            kind: CronKind::Recurring("18 9 * * 1-5".to_owned()),
            prompt: "market open".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        let key = forge_workspace::SessionSlot::from_str_for_test("web-api-uuid");
        let mut bucket = crate::app::session::UiSession::new(key.clone(), "web-api");
        bucket.cwd_raw = "~/Projects/web-api".to_owned();
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(
            app.forge_crons,
            vec![cron],
            "SCHEDULES resolves via the stamped project name regardless of the cwd form",
        );
    }

    /// The bucket's own stamp resolves the project while the catalog does
    /// not name the session yet - the spawn window, before the scan picks
    /// the new id up. A bucket whose stamp names no project - not in the
    /// catalog, not a known name - still degrades cleanly to empty rather
    /// than surfacing another project's crons.
    #[test]
    fn refresh_forge_crons_resolves_by_the_bucket_stamp_before_the_catalog() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");

        ws.seed_test_project("cronproj", "/tmp/cronproj-inspector");
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "cronproj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        // A session the catalog does not know yet: the bucket's stamp is
        // what names the project.
        let fresh = forge_workspace::SessionSlot::from_str_for_test("fresh-uuid");
        let bucket = crate::app::session::UiSession::new(fresh.clone(), "cronproj");
        app.sessions.insert(fresh.clone(), bucket);
        app.active_session_key = Some(fresh);

        app.refresh_forge_crons();
        assert_eq!(
            app.forge_crons,
            vec![cron],
            "the bucket's stamp resolves the project before the catalog does",
        );

        // Degrade cleanly: an active bucket that resolves via no link (not
        // a known project name, not catalogued, a stamp naming no project,
        // cwd under no project) yields empty rather than another
        // project's crons.
        let orphan = forge_workspace::SessionSlot::from_str_for_test("orphan-uuid");
        let mut orphan_bucket =
            crate::app::session::UiSession::new(orphan.clone(), "orphan-project");
        orphan_bucket.cwd_raw = "/tmp/unmapped-dir".to_owned();
        app.sessions.insert(orphan.clone(), orphan_bucket);
        app.active_session_key = Some(orphan);

        app.refresh_forge_crons();
        assert!(app.forge_crons.is_empty(), "an unresolvable active bucket yields empty SCHEDULES");
    }

    /// The Inspector scopes SCHEDULES by the stamped project name, so it
    /// surfaces the project's crons whatever the active session key is -
    /// a real claude UUID (project lead) or a worker's key. The bucket
    /// cwd is left blank to prove the resolution does not depend on it.
    #[test]
    fn refresh_forge_crons_resolves_across_active_key_shapes() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "cronproj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };

        for key_str in ["11111111-2222-3333-4444-555555555555", "worker-uuid"] {
            let mut app = App::test_default();
            let ws = app.workspace.clone().expect("test workspace");
            ws.seed_test_project("cronproj", "/tmp/cronproj-shapes");
            ws.seed_test_cron(cron.clone());

            let key = forge_workspace::SessionSlot::from_str_for_test(key_str);
            let bucket = crate::app::session::UiSession::new(key.clone(), "cronproj");
            app.sessions.insert(key.clone(), bucket);
            app.active_session_key = Some(key);

            app.refresh_forge_crons();
            assert_eq!(
                app.forge_crons,
                vec![cron.clone()],
                "active key {key_str} resolves the project's crons via the stamped name",
            );
        }
    }

    /// A worker spawned into a git worktree carries the worktree path
    /// (`<project>/.claude/worktrees/<label>`) as its cwd, but its bucket
    /// is stamped with the PARENT project name (resolved at Connect), so
    /// the Inspector surfaces the parent project's crons.
    #[test]
    fn refresh_forge_crons_resolves_worktree_worker_via_parent_project() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/tmp/cronproj-worktree";
        ws.seed_test_project("cronproj", path);
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "cronproj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        let key = forge_workspace::SessionSlot::from_str_for_test("worktree-worker-uuid");
        let mut bucket = crate::app::session::UiSession::new(key.clone(), "cronproj");
        bucket.cwd_raw = format!("{path}/.claude/worktrees/reviewer");
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(
            app.forge_crons,
            vec![cron],
            "a worktree worker's Inspector resolves its parent project's crons",
        );
    }

    /// GOTIFY mirrors SCHEDULES: the Inspector scopes the active project's
    /// subscriptions by the stamped project name, regardless of the
    /// bucket cwd (here a worktree path that does not equal the project
    /// root).
    #[test]
    fn refresh_gotify_scopes_by_stamped_project_name() {
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/tmp/gotify-inspector-proj";
        ws.seed_test_project("gproj", path);
        ws.seed_test_gotify_subscription(forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: "gproj".to_owned(),
            team_role: None,
            applications: vec!["alerts".to_owned()],
            min_priority: Some(5),
            created_at: std::time::SystemTime::UNIX_EPOCH,
        });

        let key = forge_workspace::SessionSlot::from_str_for_test("gproj-uuid");
        let mut bucket = crate::app::session::UiSession::new(key.clone(), "gproj");
        bucket.cwd_raw = format!("{path}/.claude/worktrees/reviewer");
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_gotify();
        assert_eq!(app.gotify_subs.len(), 1, "GOTIFY resolves subscriptions via the stamped name");
    }

    // ---------------------------------------------------------
    // Own-scope: SCHEDULES + GOTIFY show only the active session's
    // own items. A lead owns the `team_role: None` set, a worker its
    // own label's; neither sees the other's.
    // ---------------------------------------------------------

    /// An App whose active tab is `session_id`, stamped with a freshly
    /// seeded `project`. The session is a lead until
    /// [`seed_live_worker`] registers it.
    fn app_on_project(
        project: &str,
        session_id: &str,
    ) -> (App, Arc<forge_workspace::Workspace>, forge_workspace::SessionSlot) {
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project(project, &format!("/tmp/{project}"));
        let key = forge_workspace::SessionSlot::from_str_for_test(session_id);
        let bucket = crate::app::session::UiSession::new(key.clone(), project);
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key.clone());
        (app, ws, key)
    }

    /// Register `session` as the live worker `label` of `project`, the
    /// state `worker_lookup_for_session` reads to resolve a session's
    /// own role. Workers never reach the sessions catalog, so this
    /// registry is the only source.
    fn seed_live_worker(
        ws: &forge_workspace::Workspace,
        project: &str,
        label: &str,
        session: &forge_workspace::SessionSlot,
    ) {
        let project_key =
            ws.list_projects().into_iter().find(|p| p.name == project).expect("seeded project").key;
        ws.insert_live_worker(
            &project_key,
            forge_workspace::WorkerEntry {
                label: label.to_owned(),
                charter: "charter".to_owned(),
                slot: session.clone(),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: forge_workspace::SessionSlot::from_str_for_test("lead"),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
    }

    fn cron_owned_by(
        id: &str,
        project: &str,
        team_role: Option<&str>,
    ) -> forge_primitives::CronEntry {
        forge_primitives::CronEntry {
            id: forge_primitives::cron::CronId::from(id),
            project_name: project.to_owned(),
            kind: forge_primitives::cron::CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            description: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: team_role.map(str::to_owned),
        }
    }

    fn sub_owned_by(
        id: u128,
        project: &str,
        team_role: Option<&str>,
    ) -> forge_primitives::GotifySubscription {
        forge_primitives::GotifySubscription {
            id: uuid::Uuid::from_u128(id),
            project: project.to_owned(),
            team_role: team_role.map(str::to_owned),
            applications: vec!["alerts".to_owned()],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn cron_ids(app: &App) -> Vec<&str> {
        app.forge_crons.iter().map(|c| c.id.as_str()).collect()
    }

    fn sub_ids(app: &App) -> Vec<u128> {
        app.gotify_subs.iter().map(|s| s.id.as_u128()).collect()
    }

    #[test]
    fn refresh_forge_crons_shows_only_the_leads_own_crons() {
        let (mut app, ws, _) = app_on_project("scoped", "lead-uuid");
        ws.seed_test_cron(cron_owned_by("lead-c", "scoped", None));
        ws.seed_test_cron(cron_owned_by("worker-c", "scoped", Some("steward")));

        app.refresh_forge_crons();

        assert_eq!(cron_ids(&app), vec!["lead-c"], "a lead sees only lead-created crons");
        assert_eq!(app.forge_schedule_rows.len(), 1, "the humanized rows match the scoped set");
    }

    #[test]
    fn refresh_forge_crons_shows_only_the_workers_own_crons() {
        let (mut app, ws, key) = app_on_project("scoped", "steward-uuid");
        seed_live_worker(&ws, "scoped", "steward", &key);
        ws.seed_test_cron(cron_owned_by("lead-c", "scoped", None));
        ws.seed_test_cron(cron_owned_by("steward-c", "scoped", Some("steward")));
        ws.seed_test_cron(cron_owned_by("reviewer-c", "scoped", Some("reviewer")));

        app.refresh_forge_crons();

        assert_eq!(
            cron_ids(&app),
            vec!["steward-c"],
            "a worker sees neither the lead's crons nor a sibling worker's",
        );
    }

    #[test]
    fn refresh_gotify_shows_only_the_leads_own_subscriptions() {
        let (mut app, ws, _) = app_on_project("scoped", "lead-uuid");
        ws.seed_test_gotify_subscription(sub_owned_by(1, "scoped", None));
        ws.seed_test_gotify_subscription(sub_owned_by(2, "scoped", Some("steward")));

        app.refresh_gotify();

        assert_eq!(sub_ids(&app), vec![1], "a lead sees only lead-created subscriptions");
    }

    #[test]
    fn refresh_gotify_shows_only_the_workers_own_subscriptions() {
        let (mut app, ws, key) = app_on_project("scoped", "steward-uuid");
        seed_live_worker(&ws, "scoped", "steward", &key);
        ws.seed_test_gotify_subscription(sub_owned_by(1, "scoped", None));
        ws.seed_test_gotify_subscription(sub_owned_by(2, "scoped", Some("steward")));
        ws.seed_test_gotify_subscription(sub_owned_by(3, "scoped", Some("reviewer")));

        app.refresh_gotify();

        assert_eq!(
            sub_ids(&app),
            vec![2],
            "a worker sees neither the lead's subscriptions nor a sibling worker's",
        );
    }

    /// A session that created nothing leaves both caches empty, which is
    /// what makes the Inspector omit both sections rather than draw a
    /// bare header.
    #[test]
    fn refresh_leaves_both_caches_empty_for_a_session_owning_nothing() {
        let (mut app, ws, key) = app_on_project("scoped", "steward-uuid");
        seed_live_worker(&ws, "scoped", "steward", &key);
        ws.seed_test_cron(cron_owned_by("lead-c", "scoped", None));
        ws.seed_test_gotify_subscription(sub_owned_by(1, "scoped", None));

        app.refresh_forge_crons();
        app.refresh_gotify();

        assert!(app.forge_crons.is_empty(), "no owned cron leaves the SCHEDULES cache empty");
        assert!(app.forge_schedule_rows.is_empty(), "and no humanized rows to render");
        assert!(app.gotify_subs.is_empty(), "no owned subscription leaves the GOTIFY cache empty");
    }

    // -----------------------------------------------------------
    // #302 redux: replay-orphan Schedule entries. Mirror of the
    // Monitor orphan-suppression pattern above. The CLI kills
    // session-only wakeups at session close, but the persisted
    // ScheduleEntry replays on resume - without this guard, the
    // SCHEDULES section surfaces phantoms.
    // -----------------------------------------------------------

    #[test]
    fn upsert_wakeup_during_replay_is_suppressed() {
        let mut app = make_test_app();
        app.replay_in_progress = true;
        let fire_at = std::time::SystemTime::now() + std::time::Duration::from_secs(60);

        app.upsert_wakeup_from_tool_input("loop poll", fire_at);

        assert!(
            app.schedules().is_empty(),
            "wakeups replayed during resume must NOT push an entry; got: {:?}",
            app.schedules()
        );
    }

    #[test]
    fn upsert_wakeup_outside_replay_pushes_normally() {
        let mut app = make_test_app();
        assert!(!app.replay_in_progress, "live default");
        let fire_at = std::time::SystemTime::now() + std::time::Duration::from_secs(60);

        app.upsert_wakeup_from_tool_input("poll", fire_at);

        assert_eq!(
            app.schedules().len(),
            1,
            "live wakeups push normally; got: {:?}",
            app.schedules()
        );
    }
}
