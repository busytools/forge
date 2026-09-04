//! The multi-session bucket model on `App`: which session is active,
//! how a switch re-derives the status mirror, the pre-Connect synthetic
//! key, and the Inspector's NEEDS ATTENTION scan.

use std::collections::HashMap;

use super::types::{AppStatus, AttentionEntry, AttentionKind, SessionUsageState};
use crate::agent::model;

impl super::App {
    /// Synthetic session key used during the pre-Connect window
    /// (test contexts and the brief startup interval before the
    /// first `Connected` event lands). [`Self::set_session_id`]
    /// migrates the bucket onto the real session key when the
    /// claude-issued id arrives.
    pub(crate) const PRE_CONNECT_KEY: &'static str = "__conn_pending__";

    /// Returns a reference to the currently-active session bucket,
    /// or `None` in the brief pre-Connect window before any session
    /// has landed in [`Self::sessions`].
    pub fn active_session(&self) -> Option<&crate::app::session::UiSession> {
        self.active_session_key.as_ref().and_then(|key| self.sessions.get(key))
    }

    /// Mutable accessor for the active session bucket.
    pub fn try_active_bucket_mut(&mut self) -> Option<&mut crate::app::session::UiSession> {
        let key = self.active_session_key.clone()?;
        self.sessions.get_mut(&key)
    }

    /// Lookup a session by key (used by the event multiplexer to
    /// route background-session events to their bucket).
    pub fn session_mut(
        &mut self,
        key: &forge_workspace::SessionKey,
    ) -> Option<&mut crate::app::session::UiSession> {
        self.sessions.get_mut(key)
    }

    /// Find the LEAD session bucket whose `cwd_raw` matches `path`.
    /// Used by the launchpad-click and projects-pane-click handlers to
    /// land the user on the resumed bucket for a project.
    ///
    /// Workers spawned via mcp__forge__workers__spawn share the
    /// project's `cwd_raw`, so a naive iter().find() can return a
    /// worker bucket non-deterministically (HashMap order). Cross-
    /// reference workspace.live_workers and exclude any session key
    /// that appears there so the projects-pane click always returns
    /// the lead.
    pub fn find_running_bucket_for_path(&self, path: &str) -> Option<forge_workspace::SessionKey> {
        let worker_keys: std::collections::HashSet<forge_workspace::SessionKey> = self
            .workspace
            .as_ref()
            .map(|ws| ws.all_live_worker_session_keys().into_iter().collect())
            .unwrap_or_default();
        self.sessions
            .iter()
            .find(|(k, s)| s.cwd_raw.as_str() == path && !worker_keys.contains(k))
            .map(|(k, _)| k.clone())
    }

    /// Map a bucket lifecycle to the App-level status. Every focus
    /// move re-runs this (switch-in, KeyRenamed's active move, the
    /// boot id-adoption), so the mirror tracks the bucket the user
    /// lands on rather than the one they left.
    fn status_for_lifecycle(lifecycle: crate::app::session::SessionLifecycleState) -> AppStatus {
        use crate::app::session::SessionLifecycleState as L;
        match lifecycle {
            L::Spawning => AppStatus::Connecting,
            L::Running => AppStatus::Running,
            L::Sleeping | L::Idle | L::Attention | L::AuthRequired | L::Failed | L::LoggedOut => {
                AppStatus::Ready
            }
        }
    }

    /// Re-derive `App.status` from the active bucket's lifecycle.
    /// `status` is a mirror of the focused bucket, so every path that
    /// moves `active_session_key` owes one call.
    pub(crate) fn refresh_status_from_active_lifecycle(&mut self) {
        let Some(lifecycle) = self.active_session().map(|s| s.lifecycle_state) else {
            return;
        };
        let next = Self::status_for_lifecycle(lifecycle);
        if self.status != next {
            self.status = next;
            self.needs_redraw = true;
        }
    }

    /// Switch which session the renderer reads from. State on both
    /// sides is preserved (in-memory buckets in `sessions`); the
    /// next paint reflects the new active session. No-op if `key`
    /// is already active or unknown; a same-key landing still
    /// re-derives the status mirror.
    ///
    /// Drops any [`Self::pending_spawn_focus`]: landing somewhere by
    /// any route settles where the user wants to be, so a spawn they
    /// asked for earlier must not pull them back when it arrives.
    pub fn switch_active_session(&mut self, key: forge_workspace::SessionKey) {
        // Cleared before the early returns: a click that lands on the
        // session already focused is still the user settling where
        // they want to be.
        self.pending_spawn_focus = None;
        if self.active_session_key.as_ref() == Some(&key) {
            // A same-key landing still settles the mirror: a focus
            // move that skipped re-derivation can leave it stale.
            self.refresh_status_from_active_lifecycle();
            return;
        }
        if !self.sessions.contains_key(&key) {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "switch_active_session_unknown_key",
                key = ?key,
                "switch_active_session called with unknown key"
            );
            return;
        }
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "active_session_switched",
            outcome = "success",
            from = %self.active_session_key.as_ref().map_or("<none>", |k| k.as_str()),
            to = %key.as_str(),
        );

        // `App.status` is derived freshly from the destination
        // bucket's `lifecycle_state` instead of being snapshotted, so
        // a background turn that completed while the user was away
        // doesn't leave a stale `Thinking`/`Running` status on the
        // incoming bucket. Input state lives on each `UiSession`, so
        // switching `active_session_key` naturally swaps the editor
        // - no draft snapshot/restore needed.
        let incoming_lifecycle = self
            .sessions
            .get(&key)
            .map_or(crate::app::session::SessionLifecycleState::Idle, |s| s.lifecycle_state);
        // Switching in IS attending: the incoming chat carries the error
        // block, so drop the attention entry rather than let it reappear
        // when the user switches away again.
        if let Some(bucket) = self.sessions.get_mut(&key) {
            bucket.failed_turn = None;
        }
        self.active_session_key = Some(key);
        self.status = Self::status_for_lifecycle(incoming_lifecycle);
        // Update terminal/tab title immediately on switch so the host
        // terminal reflects the project the user just selected. The
        // render-loop's tab-title call (in `app::run`) only fires
        // every animating frame or on explicit `needs_redraw`
        // transitions; some terminals coalesce/debounce OSC 2 titles
        // when fired close together, so calling here directly with
        // the incoming bucket's cwd guarantees one canonical update
        // per switch.
        crate::app::tab_title::update_tab_title(
            self.shows_activity(),
            self.spinner_frame,
            self.cwd(),
        );
        // Ensure the file index for `@`-mention autocomplete is
        // started for the incoming bucket. Each bucket owns its own
        // `FileIndexState`; if this is the first time we've switched
        // to this bucket the index is empty and needs a fresh scan
        // against the bucket's cwd. `ensure_started` is idempotent:
        // it's a no-op when the bucket's index is already scanning
        // or has a current root matching the cwd.
        crate::app::file_index::ensure_started(self);
        // No explicit git-diff refresh on session switch - the 10s
        // timer (which fires its first tick immediately) catches any
        // stale snapshot on the next pump cycle.
        //
        // Activation parity with the chat-direct path
        // (`forge <project>`). That path lands the user in a fully
        // wired session via `apply_connected_presentation`'s active
        // branch - file index restart, chat focus rebuild, runtime
        // tabs refresh, the same per-session refresh chain. The
        // launchpad-pick path spawns the project in the BACKGROUND
        // branch (because `__conn_pending__` is still active at
        // Connected time) and then relies on `switch_active_session`
        // to bring the bucket up to the same activation level.
        // Without these calls clicking forge from the launchpad
        // leaves the chat input unfocused, the runtime tabs stale,
        // and the bottom panel bars empty even though the bucket
        // itself carries the data.
        crate::app::file_index::restart(self);
        self.rebuild_chat_focus_from_state();
        crate::app::config::refresh_runtime_tabs_for_session_change(self);
        crate::app::session_runtime::request_status_snapshot_refresh(self);
        crate::app::session_runtime::request_oauth_credentials_snapshot_refresh(self);
        crate::app::session_runtime::request_context_usage_refresh(self);
        crate::app::usage::request_refresh_if_needed(self);
        self.sync_welcome_snapshot();
        self.force_redraw = true;
        self.needs_redraw = true;
    }

    /// Internal helper: yield a `&mut Session` for the active bucket,
    /// auto-creating a pre-Connect synthetic bucket if no active
    /// session exists. Used by the `_mut` accessors so call sites
    /// can stay infallible.
    ///
    /// Hot path: chat render and ~50 other `_mut` accessors hit this
    /// per frame. Uses the `HashMap::entry` API to avoid the extra
    /// `SessionKey` clone an `if !contains { insert }` shape would
    /// need.
    pub(super) fn active_bucket_mut(&mut self) -> &mut crate::app::session::UiSession {
        use std::collections::hash_map::Entry;
        // The active key is normally already set; the synthetic
        // fallback is the cold first-touch path.
        let key = if let Some(key) = self.active_session_key.clone() {
            key
        } else {
            let synthetic = forge_workspace::SessionKey::from_session_id(Self::PRE_CONNECT_KEY);
            self.active_session_key = Some(synthetic.clone());
            synthetic
        };
        match self.sessions.entry(key) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let new = crate::app::session::UiSession::new(e.key().clone());
                e.insert(new)
            }
        }
    }

    /// Active session's claude session id, or `None` in the
    /// pre-Connect window.
    ///
    /// Workspace keeps an internal copy on `DomainSession.session_id`
    /// for `AgentHandle` dispatch; TUI mirrors that id onto the
    /// active bucket via `set_session_id`. This accessor reads the
    /// TUI mirror so render code doesn't need to lock the workspace.
    pub fn session_id(&self) -> Option<model::SessionId> {
        self.active_session()
            .and_then(|s| s.session_id.as_ref())
            .map(|sid| model::SessionId::new(sid.to_string()))
    }

    /// Set the active session's session_id. Ensures the sessions
    /// map has an entry keyed by the id; sets `active_session_key`
    /// to that entry.
    ///
    /// `id = None` clears the active bucket's `session_id` and
    /// `key` fields but leaves the bucket attached to
    /// `active_session_key`. The active-path event handlers
    /// (`auth_required`, `connection_failed`) call this from inside
    /// a longer cleanup sequence that still needs to write into the
    /// active bucket - finalizing in-flight tool calls to Failed,
    /// pushing system messages - so the user can see what happened.
    /// Removing the bucket here would orphan that work into a
    /// freshly-minted pre-Connect bucket.
    ///
    /// If a synthetic-keyed bucket exists (from an earlier
    /// `install_testing_stub` before `set_session_id` - test ordering),
    /// migrates that bucket's contents to the real key so the conn
    /// + session_id end up on the same bucket.
    ///
    /// Leak guard: when `active_session_key` was previously `None`
    /// (Connect-after-failure path), sweeps stale buckets from
    /// earlier disconnect cycles.
    pub fn set_session_id(&mut self, id: Option<model::SessionId>) {
        if let Some(id) = id {
            {
                let prev_active_was_none = self.active_session_key.is_none();
                let key = forge_workspace::SessionKey::from_session_id(id.to_string());
                let primitive_id = forge_primitives::SessionId::new(id.to_string());
                // Migrate any synthetic-keyed bucket onto the real key.
                // Guard against the case where BOTH a synthetic bucket
                // and the real-key bucket already exist: in that case
                // the real bucket is authoritative, and we must NOT
                // overwrite it with the synthetic. Stamp the real
                // bucket's session_id and drop the synthetic.
                let pending = forge_workspace::SessionKey::from_session_id(Self::PRE_CONNECT_KEY);
                if let Some(mut existing) = self.sessions.remove(&pending) {
                    if self.sessions.contains_key(&key) {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "set_session_id_synthetic_dropped",
                            message = "synthetic pre-Connect bucket dropped because the real-key bucket already existed",
                            outcome = "dropped",
                            session_id = %id,
                            reason = "real_bucket_present",
                        );
                        drop(existing);
                    } else {
                        existing.key = Some(key.clone());
                        existing.session_id = Some(primitive_id.clone());
                        self.sessions.insert(key.clone(), existing);
                        // Mirror the bucket re-key onto the workspace's
                        // `DomainSession` handle map so the workspace's
                        // `domain_session_for(real_key)` lookup keeps
                        // resolving after the synthetic→real migration.
                        if let Some(ws) = self.workspace.as_ref() {
                            ws.rekey_domain_session(&pending, key.clone());
                        }
                    }
                } else {
                    let bucket = self
                        .sessions
                        .entry(key.clone())
                        .or_insert_with(|| crate::app::session::UiSession::new(key.clone()));
                    bucket.session_id = Some(primitive_id.clone());
                }
                self.active_session_key = Some(key.clone());
                self.refresh_status_from_active_lifecycle();
                // Mirror `session_id` onto the workspace's
                // DomainSession so `AgentHandle` dispatch (which
                // routes by claude-issued session UUID) finds it.
                // Auto-create a handle-less domain when the workspace
                // doesn't yet have one for `key` - covers the rare
                // test path that calls `set_session_id` before any
                // domain is registered.
                if let Some(ws) = self.workspace.as_ref() {
                    if ws.domain_session_for(&key).is_none() {
                        ws.register_domain_session(key.clone(), None);
                    }
                    ws.set_session_id_in_domain(&key, Some(primitive_id));
                }
                // If we landed on an existing bucket without going
                // through the synthetic-migration branch above, ensure
                // its `session_id` mirror is current.
                if let Some(bucket) = self.sessions.get_mut(&key) {
                    bucket.session_id = Some(forge_primitives::SessionId::new(id.to_string()));
                }
                // Connect-after-failure cleanup: when no session was
                // active before this call, sweep stale buckets that
                // accumulated across earlier disconnect cycles.
                if prev_active_was_none {
                    self.sessions.retain(|k, _| *k == key);
                }
            }
        } else {
            // Clear the bucket's `key` field so it stops
            // advertising the now-stale id (the next
            // `set_session_id(Some(...))` re-stamps it). Keep
            // the bucket attached to `active_session_key` so
            // the active-path handler can keep writing into it
            // (failed tool calls, system messages - see doc
            // comment above). Also clear the workspace's
            // DomainSession session_id so readers observe `None`.
            if let Some(s) = self.try_active_bucket_mut() {
                s.key = None;
                s.session_id = None;
            }
            if let Some(ws) = self.workspace.as_ref()
                && let Some(key) = self.active_session_key.as_ref()
            {
                ws.set_session_id_in_domain(key, None);
            }
        }
    }

    pub fn clear_session_runtime_identity(&mut self) {
        self.set_session_id(None);
        self.set_current_model(None);
        self.set_observed_assistant_model(None);
        self.set_mode(None);
        self.set_runtime_session_state(None);
        self.set_observed_permission_mode(None);
        self.set_observed_effort(None);
        self.set_pending_mode_rollback(None);
        self.set_pending_model_rollback(None);
        *self.session_usage_mut() = SessionUsageState::default();
        let bucket = self.active_bucket_mut();
        bucket.dictate_overrides = forge_workspace::DictateOverrides::default();
        bucket.dictate_device_pin = None;
    }

    /// The active tab's forge.toml project name, backing the Inspector
    /// SCHEDULES + GOTIFY snapshots. Resolved through a robust chain so a
    /// missing / lost per-bucket stamp can't blank the section while the
    /// rest of the tab (GIT, PROCESSES, the pane highlight, the top bar)
    /// renders the project fine:
    ///   1. `resolve_active_project_view` on the active KEY - the exact
    ///      resolver the projects pane + top bar use (catalog for a real
    ///      UUID, name for a `__spawn_<name>__` sentinel). Independent of
    ///      the stamp, so it resolves whenever the pane highlights the
    ///      project.
    ///   2. The per-bucket stamp (`UiSession.project`, set at Connect).
    ///   3. `project_name_for_path(cwd_raw)` - resolve from the active
    ///      bucket's cwd, the same value GIT/PROCESSES read successfully.
    pub fn active_project_name(&self) -> Option<String> {
        let active_key = self.active_session_key.as_ref()?;
        if let Some(ws) = self.workspace.as_ref() {
            let projects = ws.list_projects();
            let refs: Vec<&forge_workspace::ProjectView> = projects.iter().collect();
            if let Some(view) =
                crate::ui::projects_pane::resolve_active_project_view(active_key, &refs)
            {
                return Some(view.name.clone());
            }
        }
        if let Some(name) = self.active_session().and_then(|s| s.project.clone()) {
            return Some(name);
        }
        let cwd = self.active_session().map(|s| s.cwd_raw.clone())?;
        self.workspace.as_ref().and_then(|ws| ws.project_name_for_path(&cwd))
    }

    /// Background sessions (everything but the active one) that need
    /// the user: a prompt pending at the head of their queue, a turn
    /// that died, or unread worker answers on their review comments.
    /// [`AttentionEntry`] rows sorted stalest-first (oldest first,
    /// session id as the tiebreaker). The first two mirror the
    /// Projects-pane glyph predicates so those surfaces never disagree.
    /// Empty when nothing needs attention - the Inspector NEEDS
    /// ATTENTION band hides on empty.
    pub fn needs_attention_sessions(&self) -> Vec<AttentionEntry> {
        let active = self.active_session_key.as_ref();
        // Cheap first pass: which background sessions need the user? The
        // common case (nothing waiting) returns here without touching the
        // workspace - no `list_projects` clone, no live-workers lock -
        // since this runs on every inspector render.
        let waiting: Vec<(&forge_workspace::SessionKey, &crate::app::session::UiSession)> = self
            .sessions
            .iter()
            .filter(|(key, session)| active != Some(*key) && session_needs_attention(session))
            .collect();
        if waiting.is_empty() {
            return Vec::new();
        }

        // At least one session is waiting: resolve names / roles. One
        // (project, role) row per live worker so the per-session lookup
        // is a map hit, not a nested per-project scan.
        let projects = self.workspace.as_ref().map(|ws| ws.list_projects()).unwrap_or_default();
        let mut worker_index: HashMap<forge_workspace::SessionKey, (String, String)> =
            HashMap::new();
        if let Some(ws) = self.workspace.as_ref() {
            for project in &projects {
                for worker in ws.list_live_workers(&project.key) {
                    worker_index.insert(
                        worker.session_key.clone(),
                        (project.name.clone(), worker.label.clone()),
                    );
                }
            }
        }
        let project_refs: Vec<&forge_workspace::ProjectView> = projects.iter().collect();

        let mut entries: Vec<AttentionEntry> = Vec::with_capacity(waiting.len());
        for (key, session) in waiting {
            // One row per session. A dead turn outranks a pending prompt:
            // the prompt can no longer be answered (its oneshot died with
            // the turn), and the failure is the signal that must not be
            // missed.
            let (kind, since) = if let Some(failed) = session.failed_turn.as_ref() {
                (
                    AttentionKind::Failed { error: failed.error, status: failed.status },
                    failed.failed_at,
                )
            } else if let Some(prompt) = session.prompt_queue.front() {
                let kind = match &prompt.source {
                    crate::app::prompt::PromptSource::Permission { tool_name, .. } => {
                        AttentionKind::Permission { tool: tool_name.clone() }
                    }
                    crate::app::prompt::PromptSource::Question { .. } => AttentionKind::Question,
                };
                (kind, prompt.enqueued_at)
            } else if let Some(replies) = session.review_replies_waiting.as_ref() {
                (AttentionKind::ReviewReplies { count: replies.count }, replies.since)
            } else {
                continue;
            };
            let (name, role) = if let Some((project_name, label)) = worker_index.get(key) {
                (project_name.clone(), Some(label.clone()))
            } else {
                let name =
                    crate::ui::projects_pane::resolve_active_project_view(key, &project_refs)
                        .map(|view| view.name.clone())
                        .or_else(|| session.project.clone())
                        .unwrap_or_else(|| key.as_str().to_owned());
                (name, None)
            };
            entries.push(AttentionEntry {
                session_key: key.clone(),
                name,
                role,
                kind,
                enqueued_at: since,
            });
        }
        entries.sort_by(|a, b| {
            a.enqueued_at
                .cmp(&b.enqueued_at)
                .then_with(|| a.session_key.as_str().cmp(b.session_key.as_str()))
        });
        entries
    }

    /// Active session's files-accessed counter.
    pub fn files_accessed(&self) -> usize {
        self.active_session().map_or(0, |s| s.files_accessed)
    }

    /// Set the active session's files-accessed counter.
    pub fn set_files_accessed(&mut self, value: usize) {
        self.active_bucket_mut().files_accessed = value;
    }

    /// Retain the classification of the active session's latest
    /// `api_retry`, so a turn error following exhausted retries can name
    /// what killed it.
    pub(crate) fn set_last_api_retry(
        &mut self,
        retry: Option<(forge_primitives::ApiRetryError, Option<u16>)>,
    ) {
        self.active_bucket_mut().last_api_retry = retry;
    }

    /// Increment the active session's files-accessed counter by one.
    pub fn increment_files_accessed(&mut self) {
        let s = self.active_bucket_mut();
        s.files_accessed = s.files_accessed.saturating_add(1);
    }
}

/// Whether a background session belongs in the NEEDS ATTENTION band.
/// Three field reads and nothing else: this is
/// [`super::App::needs_attention_sessions`]'s first pass, which runs on
/// every inspector frame and must fall through without touching the
/// workspace when nothing is waiting.
fn session_needs_attention(session: &crate::app::session::UiSession) -> bool {
    !session.prompt_queue.is_empty()
        || session.failed_turn.is_some()
        || session.review_replies_waiting.is_some()
}

#[cfg(test)]
mod tests {
    use super::super::{App, AppStatus};
    use pretty_assertions::assert_eq;

    // ── needs_attention_sessions (Inspector NEEDS ATTENTION band) ──────────

    /// Seed a background session carrying one pending permission prompt
    /// enqueued `secs` after the UNIX epoch; stamps a project name so
    /// the row resolves without a workspace catalog. Returns its key.
    fn seed_attention_session(app: &mut App, id: &str, secs: u64) -> forge_workspace::SessionKey {
        let key = forge_workspace::SessionKey::from_session_id(id);
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some(format!("proj-{id}"));
        let mut prompt = crate::app::prompt::PromptState::from_permission(
            format!("tc-{id}"),
            crate::app::prompt::tests::make_permission_request(),
        );
        prompt.enqueued_at =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        session.prompt_queue.push_back(prompt);
        app.sessions.insert(key.clone(), session);
        key
    }

    #[test]
    fn needs_input_sessions_empty_when_no_prompts() {
        let app = App::test_default();
        assert!(app.needs_attention_sessions().is_empty(), "no pending prompts -> no rows");
    }

    /// The band's first pass runs on every inspector frame, so a settled
    /// session must fall out of it on field reads alone - that is what
    /// lets `needs_attention_sessions` return before it clones
    /// `list_projects` or takes the live-workers lock.
    #[test]
    fn attention_first_pass_ignores_a_settled_session() {
        let settled = crate::app::session::UiSession::new(
            forge_workspace::SessionKey::from_session_id("quiet"),
        );
        assert!(
            !super::session_needs_attention(&settled),
            "nothing waiting -> not a band candidate"
        );

        let mut waiting = crate::app::session::UiSession::new(
            forge_workspace::SessionKey::from_session_id("quiet"),
        );
        waiting.review_replies_waiting = Some(crate::app::ReviewRepliesWaiting {
            branch: "feat".to_owned(),
            count: 1,
            since: std::time::SystemTime::UNIX_EPOCH,
        });
        assert!(super::session_needs_attention(&waiting), "unread worker answers are a candidate");
    }

    #[test]
    fn needs_input_sessions_includes_background_and_excludes_active() {
        let mut app = App::test_default();
        seed_attention_session(&mut app, "bg", 100);
        let active = seed_attention_session(&mut app, "active", 50);
        app.active_session_key = Some(active);

        let entries = app.needs_attention_sessions();
        let keys: Vec<&str> = entries.iter().map(|e| e.session_key.as_str()).collect();
        assert_eq!(keys, vec!["bg"], "the active session is excluded even with a pending prompt");
        assert!(matches!(entries[0].kind, crate::app::AttentionKind::Permission { .. }));
    }

    #[test]
    fn needs_input_sessions_sorted_stalest_first() {
        let mut app = App::test_default();
        // Insert newest-first to prove the sort reorders by enqueue age,
        // not by insertion order.
        seed_attention_session(&mut app, "newest", 300);
        seed_attention_session(&mut app, "oldest", 100);
        seed_attention_session(&mut app, "middle", 200);
        let entries = app.needs_attention_sessions();
        let order: Vec<&str> = entries.iter().map(|e| e.session_key.as_str()).collect();
        assert_eq!(order, vec!["oldest", "middle", "newest"], "stalest (oldest enqueue) on top");
    }

    #[test]
    fn needs_input_sessions_reports_question_kind() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("q");
        let mut session = crate::app::session::UiSession::new(key.clone());
        let mut prompt = crate::app::prompt::PromptState::from_question(
            "tc-q".to_owned(),
            crate::app::prompt::tests::make_question_request(false),
        );
        prompt.enqueued_at = std::time::SystemTime::UNIX_EPOCH;
        session.prompt_queue.push_back(prompt);
        app.sessions.insert(key, session);
        let entries = app.needs_attention_sessions();
        assert!(
            entries.iter().any(|e| matches!(e.kind, crate::app::AttentionKind::Question)),
            "a session with an AskUserQuestion prompt reports the Question kind",
        );
    }

    #[test]
    fn needs_input_sessions_tiebreaks_equal_enqueue_by_session_id() {
        // `sessions` is a HashMap (unordered iteration), so equal enqueue
        // times must resolve deterministically via the session-id
        // tiebreak or the band would flicker order between frames. Seed
        // in reverse id order to prove the sort, not insertion order.
        let mut app = App::test_default();
        seed_attention_session(&mut app, "zeta", 500);
        seed_attention_session(&mut app, "alpha", 500);
        let entries = app.needs_attention_sessions();
        let order: Vec<&str> = entries.iter().map(|e| e.session_key.as_str()).collect();
        assert_eq!(order, vec!["alpha", "zeta"], "equal enqueue -> deterministic id tiebreak");
    }

    #[test]
    fn needs_input_sessions_resolves_worker_role_from_live_worker() {
        use forge_workspace::{SessionKey, WorkerEntry};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project("core-v1", "/tmp/core-v1");
        // Insert the worker under the SAME key list_projects returns
        // (derived from the project path, not the name).
        let project_key = ws
            .list_projects()
            .into_iter()
            .find(|p| p.name == "core-v1")
            .expect("seeded project")
            .key;
        let worker_key = SessionKey::from_session_id("worker-steward-uuid");
        ws.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "steward".into(),
                charter: "be sharp".into(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        // A waiting (background) bucket for that worker session.
        let mut session = crate::app::session::UiSession::new(worker_key.clone());
        let mut prompt = crate::app::prompt::PromptState::from_permission(
            "tc-w".to_owned(),
            crate::app::prompt::tests::make_permission_request(),
        );
        prompt.enqueued_at = std::time::SystemTime::UNIX_EPOCH;
        session.prompt_queue.push_back(prompt);
        app.sessions.insert(worker_key.clone(), session);

        let entries = app.needs_attention_sessions();
        let entry = entries.iter().find(|e| e.session_key == worker_key).expect("worker entry");
        assert_eq!(entry.name, "core-v1", "name resolves to the owning project");
        assert_eq!(entry.role.as_deref(), Some("steward"), "role resolves to the worker label");
    }

    // ── failed-turn attention rows ─────────────────────────────────

    /// Seed a background session whose last turn failed `secs` after the
    /// epoch with the given classification. Returns its key.
    fn seed_failed_session(
        app: &mut App,
        id: &str,
        secs: u64,
        error: forge_primitives::ApiRetryError,
        status: Option<u16>,
    ) -> forge_workspace::SessionKey {
        let key = forge_workspace::SessionKey::from_session_id(id);
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some(format!("proj-{id}"));
        session.failed_turn = Some(crate::app::FailedTurn {
            error,
            status,
            failed_at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        });
        app.sessions.insert(key.clone(), session);
        key
    }

    #[test]
    fn needs_attention_sessions_includes_failed_background_session() {
        let mut app = App::test_default();
        let key = seed_failed_session(
            &mut app,
            "bg",
            100,
            forge_primitives::ApiRetryError::ServerError,
            Some(529),
        );

        let entries = app.needs_attention_sessions();
        let entry = entries.iter().find(|e| e.session_key == key).expect("failed row present");
        assert_eq!(
            entry.kind,
            crate::app::AttentionKind::Failed {
                error: forge_primitives::ApiRetryError::ServerError,
                status: Some(529),
            },
            "a failed background turn surfaces as a Failed attention row",
        );
    }

    #[test]
    fn needs_attention_sessions_excludes_failed_active_session() {
        let mut app = App::test_default();
        let active = seed_failed_session(
            &mut app,
            "active",
            100,
            forge_primitives::ApiRetryError::Unknown,
            None,
        );
        app.active_session_key = Some(active);
        assert!(
            app.needs_attention_sessions().is_empty(),
            "the session the user is looking at already shows its error in the chat",
        );
    }

    /// Seed a background session holding `count` unread worker answers on
    /// its review comments, waiting since `secs` after the epoch.
    fn seed_review_replies_session(
        app: &mut App,
        id: &str,
        secs: u64,
        count: usize,
    ) -> forge_workspace::SessionKey {
        let key = forge_workspace::SessionKey::from_session_id(id);
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some(format!("proj-{id}"));
        session.review_replies_waiting = Some(crate::app::ReviewRepliesWaiting {
            branch: "feat".to_owned(),
            count,
            since: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        });
        app.sessions.insert(key.clone(), session);
        key
    }

    #[test]
    fn needs_attention_sessions_includes_waiting_review_replies() {
        let mut app = App::test_default();
        let key = seed_review_replies_session(&mut app, "bg", 100, 2);

        let entries = app.needs_attention_sessions();
        let entry = entries.iter().find(|e| e.session_key == key).expect("review row present");
        assert_eq!(entry.kind, crate::app::AttentionKind::ReviewReplies { count: 2 });
        assert_eq!(
            entry.enqueued_at,
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100),
            "the row ages from when the replies landed",
        );
    }

    /// The band is about work happening ELSEWHERE - the active session's
    /// own waiting replies are the GIT header badge's job.
    #[test]
    fn needs_attention_sessions_excludes_active_sessions_review_replies() {
        let mut app = App::test_default();
        let active = seed_review_replies_session(&mut app, "active", 100, 3);
        app.active_session_key = Some(active);
        assert!(
            app.needs_attention_sessions().is_empty(),
            "the session the user is looking at gets the GIT badge instead",
        );
    }

    /// Nothing is blocked on an unread reply, so a session that is also
    /// waiting on the user shows that instead.
    #[test]
    fn needs_attention_sessions_prefers_a_pending_prompt_over_review_replies() {
        let mut app = App::test_default();
        let key = seed_attention_session(&mut app, "both", 100);
        app.sessions.get_mut(&key).expect("seeded bucket").review_replies_waiting =
            Some(crate::app::ReviewRepliesWaiting {
                branch: "feat".to_owned(),
                count: 4,
                since: std::time::SystemTime::UNIX_EPOCH,
            });

        let entries = app.needs_attention_sessions();
        assert_eq!(entries.len(), 1, "one row per session");
        assert!(
            matches!(entries[0].kind, crate::app::AttentionKind::Permission { .. }),
            "the pending prompt outranks the unread replies: {:?}",
            entries[0].kind,
        );
    }

    /// A failed turn outranks a stale pending prompt on the same session:
    /// the band emits one row per session and the error is the signal the
    /// user must not miss.
    #[test]
    fn needs_attention_sessions_prefers_failure_over_pending_prompt() {
        let mut app = App::test_default();
        let key = seed_attention_session(&mut app, "both", 100);
        app.sessions.get_mut(&key).expect("seeded bucket").failed_turn =
            Some(crate::app::FailedTurn {
                error: forge_primitives::ApiRetryError::BillingError,
                status: Some(400),
                failed_at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200),
            });

        let entries = app.needs_attention_sessions();
        assert_eq!(entries.len(), 1, "one row per session");
        assert!(
            matches!(entries[0].kind, crate::app::AttentionKind::Failed { .. }),
            "the failure wins over the pending prompt: {:?}",
            entries[0].kind,
        );
    }

    /// Switching to a failed session IS attending to it - the chat shows
    /// the error block, so the band entry must not survive to reappear
    /// the next time the user switches away.
    #[test]
    fn failed_turn_clears_when_session_becomes_active() {
        let mut app = App::test_default();
        let key = seed_failed_session(
            &mut app,
            "bg",
            100,
            forge_primitives::ApiRetryError::ServerError,
            Some(529),
        );
        // A second bucket so there is somewhere to switch away to.
        seed_failed_session(&mut app, "other", 50, forge_primitives::ApiRetryError::Unknown, None);
        app.active_session_key = Some(forge_workspace::SessionKey::from_session_id("other"));

        app.switch_active_session(key.clone());
        assert!(
            app.sessions.get(&key).expect("bucket").failed_turn.is_none(),
            "attending to the session clears its failure",
        );

        app.switch_active_session(forge_workspace::SessionKey::from_session_id("other"));
        assert!(
            !app.needs_attention_sessions().iter().any(|e| e.session_key == key),
            "the attended failure does not come back on switch-away",
        );
    }

    /// The boot id-adoption moves the active key onto the real bucket
    /// without a switch, so the status mirror must re-derive there
    /// instead of keeping the boot Connecting.
    #[test]
    fn set_session_id_adopts_status_from_the_destination_bucket() {
        let mut app = App::test_default();
        app.status = AppStatus::Connecting;
        let uuid = "11111111-2222-3333-4444-555555555555";
        let real = forge_workspace::SessionKey::from_session_id(uuid);
        let mut bucket = crate::app::session::UiSession::new(real.clone());
        bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
        app.sessions.insert(real.clone(), bucket);

        app.set_session_id(Some(crate::agent::model::SessionId::new(uuid)));

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&real),
            "adoption lands on the real bucket"
        );
        assert_eq!(
            app.status,
            AppStatus::Ready,
            "status re-derives from the adopted bucket instead of sticking at Connecting"
        );
    }

    /// A pick that lands on the already-focused session must still
    /// settle the status mirror: after the boot id-adoption the next
    /// launchpad Enter hits this path with a stale Connecting.
    #[test]
    fn same_key_switch_still_derives_status_from_the_bucket() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("same");
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key.clone());
        app.status = AppStatus::Connecting;

        app.switch_active_session(key);

        assert_eq!(
            app.status,
            AppStatus::Ready,
            "a same-key landing re-derives from the bucket's Idle lifecycle"
        );
    }

    /// The launchpad-stall repro end to end: the boot adoption moves
    /// focus, the user's first pick lands on the same key, and the
    /// composer must come out of the blocked-input set.
    #[test]
    fn boot_adoption_then_same_key_pick_leaves_the_composer_typable() {
        let mut app = App::test_default();
        app.status = AppStatus::Connecting;
        let real = forge_workspace::SessionKey::from_session_id("boot-resumed");
        let mut bucket = crate::app::session::UiSession::new(real.clone());
        bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
        app.sessions.insert(real.clone(), bucket);

        app.set_session_id(Some(crate::agent::model::SessionId::new("boot-resumed")));
        app.switch_active_session(real);

        assert!(
            !matches!(
                app.status,
                AppStatus::Connecting | AppStatus::CommandPending | AppStatus::Error
            ),
            "the composer must be typable after the pick, got {:?}",
            app.status
        );
    }

    /// A session that starts another turn has recovered - the stale
    /// failure row must go, whether the turn came from the user or from
    /// forge's own auto-continue.
    #[test]
    fn failed_turn_clears_when_a_new_turn_starts() {
        let mut app = App::test_default();
        let key = seed_failed_session(
            &mut app,
            "bg",
            100,
            forge_primitives::ApiRetryError::ServerError,
            Some(529),
        );
        crate::app::events::set_bucket_lifecycle_state(
            &mut app,
            &key,
            crate::app::session::SessionLifecycleState::Running,
        );
        assert!(
            app.sessions.get(&key).expect("bucket").failed_turn.is_none(),
            "a new turn on the session clears the failure",
        );
    }

    /// Going Idle is not recovery - the turn-error path itself parks the
    /// bucket at Idle, so clearing there would erase the row the instant
    /// it was set.
    #[test]
    fn failed_turn_survives_an_idle_transition() {
        let mut app = App::test_default();
        let key = seed_failed_session(
            &mut app,
            "bg",
            100,
            forge_primitives::ApiRetryError::ServerError,
            Some(529),
        );
        crate::app::events::set_bucket_lifecycle_state(
            &mut app,
            &key,
            crate::app::session::SessionLifecycleState::Idle,
        );
        assert!(
            app.sessions.get(&key).expect("bucket").failed_turn.is_some(),
            "Idle is where a failed turn parks; the row must outlive it",
        );
    }

    #[test]
    fn test_default_seeds_pre_connect_bucket_so_accessors_are_infallible() {
        let app = App::test_default();
        // Task 3 onwards: per-session field accessors (messages, viewport,
        // ...) need an active session to read/write. test_default seeds a
        // synthetic pre-Connect bucket so call sites stay infallible
        // before Connect lands.
        assert_eq!(app.sessions.len(), 1);
        let pre_connect_key = forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY);
        assert_eq!(app.active_session_key.as_ref(), Some(&pre_connect_key));
        assert!(app.active_session().is_some());
    }

    #[test]
    fn inserting_a_session_makes_it_active_via_accessors() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_str_for_test("abc-123");
        app.sessions
            .entry(key.clone())
            .or_insert_with(|| crate::app::session::UiSession::new(key.clone()));
        app.active_session_key = Some(key.clone());

        assert_eq!(app.active_session_key.as_ref(), Some(&key));
        assert!(app.active_session().is_some());
        assert_eq!(app.active_session().and_then(|s| s.key.as_ref()), Some(&key));
        assert!(app.try_active_bucket_mut().is_some());
        assert!(app.session_mut(&key).is_some());
    }

    /// `request_context_usage_refresh` flips
    /// `session_usage.context_usage_in_flight = true` when it
    /// successfully proceeds (it needs workspace + active key +
    /// session_id, all of which the destination bucket has post-switch).
    /// Observing the flag flip on the destination bucket proves
    /// `switch_active_session` invoked the refresh chain.
    #[test]
    fn switch_active_session_triggers_context_usage_refresh_on_destination() {
        let mut app = App::test_default();
        let _pre_connect_outbox = app.install_testing_stub();
        // Seed a second bucket and stamp it with a session_id so the
        // refresh fns clear their session_id gate after the switch.
        let dest_key = forge_workspace::SessionKey::from_str_for_test("destination-session");
        let mut dest_bucket = crate::app::session::UiSession::new(dest_key.clone());
        dest_bucket.session_id =
            Some(forge_primitives::SessionId::new(dest_key.as_str().to_owned()));
        app.sessions.insert(dest_key.clone(), dest_bucket);
        // Hold the destination's command receiver alive at test scope -
        // dropping it before `switch_active_session` runs makes the
        // workspace's stub-handle send fail, which routes through the
        // error arm in `request_context_usage_refresh` and resets the
        // in_flight flag we're trying to observe.
        let _dest_outbox = if let Some(workspace) = app.workspace.as_ref() {
            let (handle, outbox) = forge_workspace::Workspace::testing_stub_handle();
            let domain = workspace
                .register_domain_session(dest_key.clone(), Some(std::sync::Arc::new(handle)));
            domain.lock().session_id =
                Some(forge_primitives::SessionId::new(dest_key.as_str().to_owned()));
            Some(outbox)
        } else {
            None
        };
        // Sanity baseline: destination bucket's context-usage is idle.
        assert!(
            !app.sessions
                .get(&dest_key)
                .expect("dest bucket")
                .session_usage
                .context_usage_in_flight,
            "destination bucket should start with context_usage idle",
        );

        app.switch_active_session(dest_key.clone());

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&dest_key),
            "switch must promote destination to active",
        );
        assert!(
            app.sessions.get(&dest_key).expect("dest bucket").session_usage.context_usage_in_flight,
            "switch_active_session must call request_context_usage_refresh on the new active \
             (otherwise the launchpad-click bottom-panel bars sit empty)",
        );
    }

    /// Regression: the pre-connect bucket's `cwd_raw` must not be
    /// seeded from `std::env::current_dir()` - forge.toml is the
    /// source of truth (Hard Rule #14). In launchpad mode (no argv
    /// project), the pre-connect bucket's `cwd_raw` stays empty so
    /// it cannot collide with any project lookup. This test pins
    /// that invariant for `test_default`'s pre-connect bucket.
    #[test]
    fn test_default_pre_connect_bucket_does_not_collide_with_project_paths() {
        let app = App::test_default();
        let pre_connect_key = forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY);
        let pre_bucket = app.sessions.get(&pre_connect_key).expect("pre-connect bucket");
        // `test_default` seeds `/test` for stable rendering; production
        // launchpad-mode pre-connect uses an empty `cwd_raw`. Either
        // way, the invariant the production fix relies on is that no
        // real project's `path` ever ends up matching the pre-connect
        // bucket's `cwd_raw` - there is no way to construct a forge
        // project named `/test` and pre-connect cannot equal a real
        // project's `path` accidentally because nothing reads from
        // `current_dir()` to seed it anymore.
        assert!(
            pre_bucket.cwd_raw == "/test" || pre_bucket.cwd_raw.is_empty(),
            "pre-connect bucket should hold a sentinel cwd, got {:?}",
            pre_bucket.cwd_raw,
        );
    }

    /// `find_running_bucket_for_path` returns the unique bucket
    /// matching `path` when one exists. The pre-connect bucket
    /// never participates because its `cwd_raw` is sourced from
    /// `forge.toml`-or-empty, not from `current_dir()` - so it
    /// cannot accidentally match a real project's `path`.
    #[test]
    fn find_running_bucket_for_path_returns_matching_real_bucket() {
        let mut app = App::test_default();
        let project_path = "/Users/developer/Projects/forge";
        let real_key =
            forge_workspace::SessionKey::from_str_for_test("11111111-2222-3333-4444-555555555555");
        let mut real_bucket = crate::app::session::UiSession::new(real_key.clone());
        real_bucket.cwd_raw = project_path.to_owned();
        app.sessions.insert(real_key.clone(), real_bucket);

        let picked = app.find_running_bucket_for_path(project_path).expect("a bucket should match");
        assert_eq!(picked, real_key);
    }

    /// No bucket matches → `None`. Used by the click handler to
    /// fall through to the catalog / cold-spawn paths.
    #[test]
    fn find_running_bucket_for_path_returns_none_when_no_match() {
        let app = App::test_default();
        assert!(app.find_running_bucket_for_path("/Users/developer/Projects/forge").is_none());
    }

    /// Regression for commit 23f46b8: when a worker session shares
    /// the project's cwd_raw with the lead, `find_running_bucket_
    /// for_path` must return the lead's session_key, never the
    /// worker's. Before the fix, HashMap iteration order could
    /// surface either bucket non-deterministically and the projects-
    /// pane click landed on a worker instead of going back to the
    /// lead.
    #[test]
    fn find_running_bucket_for_path_excludes_worker_session_keys() {
        use forge_workspace::WorkerEntry;
        use forge_workspace::{ProjectKey, SessionKey};

        let mut app = App::test_default();
        let project_path = "/Users/developer/Projects/forge";

        let lead_key = SessionKey::from_str_for_test("aaaaaaaa-1111-2222-3333-444444444444");
        let worker_key = SessionKey::from_str_for_test("bbbbbbbb-1111-2222-3333-444444444444");

        let mut lead_bucket = crate::app::session::UiSession::new(lead_key.clone());
        lead_bucket.cwd_raw = project_path.to_owned();
        app.sessions.insert(lead_key.clone(), lead_bucket);

        let mut worker_bucket = crate::app::session::UiSession::new(worker_key.clone());
        worker_bucket.cwd_raw = project_path.to_owned();
        app.sessions.insert(worker_key.clone(), worker_bucket);

        // Inject the worker into the workspace's live_workers map so
        // the filter inside find_running_bucket_for_path sees it.
        let workspace = app.workspace.as_ref().expect("test_default wires a workspace");
        let project_key = ProjectKey::new_for_test("-Users-developer-Projects-forge");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "test-worker".to_owned(),
                charter: "noop".to_owned(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: lead_key.as_str().to_owned(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        let picked = app
            .find_running_bucket_for_path(project_path)
            .expect("lead bucket should match even with a worker at the same cwd");
        assert_eq!(picked, lead_key, "lead must be returned; worker must be excluded");
        assert_ne!(picked, worker_key);
    }

    #[test]
    fn clear_session_runtime_identity_resets_session_usage() {
        let mut app = App::test_default();
        app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
        app.set_current_model(Some(
            crate::agent::model::CurrentModel::new("sonnet", "Claude Sonnet", "Claude Sonnet")
                .authoritative(true),
        ));
        app.set_mode(Some(crate::app::ModeState {
            current_mode_id: "plan".to_owned(),
            current_mode_name: "Plan".to_owned(),
            available_modes: Vec::new(),
        }));
        let usage = app.session_usage_mut();
        usage.context_usage_percent = Some(62);
        usage.context_usage_in_flight = true;
        usage.context_usage_refresh_pending = Some(crate::app::state::types::RefreshPending::Auto);
        usage.last_compaction_pre_tokens = Some(123_456);
        {
            let bucket = app.active_bucket_mut();
            bucket.dictate_overrides.styling = Some(forge_workspace::Styling::Formal);
            bucket.dictate_device_pin =
                Some(forge_workspace::DictateDeviceChoice::Device("shure-id".into()));
        }

        app.clear_session_runtime_identity();

        assert!(app.session_id().is_none());
        assert!(app.current_model().is_none());
        assert!(app.mode().is_none());
        assert_eq!(*app.session_usage(), crate::app::state::types::SessionUsageState::default());
        let bucket = app.active_bucket_mut();
        assert_eq!(
            bucket.dictate_overrides,
            forge_workspace::DictateOverrides::default(),
            "a torn-down identity keeps no override mirrors"
        );
        assert_eq!(
            bucket.dictate_device_pin, None,
            "a torn-down identity keeps no device pin: the workspace holds none"
        );
    }

    #[test]
    fn clear_session_runtime_identity_clears_observed_assistant_model() {
        let mut app = App::test_default();
        app.set_observed_assistant_model(Some("claude-observed".to_owned()));

        app.clear_session_runtime_identity();

        assert!(app.observed_assistant_model().is_none());
    }
}
