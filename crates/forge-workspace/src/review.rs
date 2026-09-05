//! The review-conversation cluster on [`Workspace`]: durable
//! review-thread CRUD over the redb store, review submit / list / get,
//! worker reply + resolve, the per-turn activity drain that notifies the
//! reviewer, and the dead-branch sweep run off the startup path.
//!
//! Everything here stays on `Workspace` as a second `impl` block, so
//! every caller (the `Command` bus arms in [`crate::workspace`],
//! `session_task`'s turn-end drain, the `mcp::review` facade,
//! forge-tui's diff overlay) keeps its path. The `db`, `review_origin`
//! and `review_activity` fields these methods own are `pub(crate)` for
//! the same reason `pool` is: so this sibling module can reach them
//! without a wrapper. Store IO lives in [`crate::store::review`]; the
//! MCP tool surface in [`crate::mcp::review`].

use std::collections::HashMap;
use std::sync::Arc;

use forge_primitives::{ReviewStatus, ReviewThread};

use crate::protocol::SessionUpdate;
use crate::target::SessionKey;
use crate::workspace::Workspace;

impl Workspace {
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
                target: "forge_workspace::review",
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
                target: "forge_workspace::review",
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
                    target: "forge_workspace::review",
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
                    target: "forge_workspace::review",
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
                target: "forge_workspace::review",
                %error,
                project = %project,
                branch = %branch,
                id = %id,
                "setting a review-thread status failed",
            );
        }
    }

    /// Delete all of `(project, branch)`'s review state - threads and
    /// reviews together, in one transaction. Called on worktree teardown
    /// once the branch itself is gone, and by the boot sweep for a branch
    /// deleted since, so orphaned rows don't linger and a later branch
    /// reusing the name inherits nothing.
    pub fn delete_branch_review_state(&self, project: &str, branch: &str) {
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::review::delete_branch_state(db, project, branch)
        {
            tracing::warn!(
                target: "forge_workspace::review",
                %error,
                project = %project,
                branch = %branch,
                "deleting review state failed",
            );
        }
    }

    /// Branch refs a repo must hold before a majority-dead sweep of it is
    /// trusted.
    ///
    /// Measured against real clones, counting branch refs rather than
    /// names: a `--depth 1` or `--single-branch` clone holds two, its
    /// local head and the one remote-tracking ref it fetched, whatever
    /// those branches are called. A full clone of a four-branch repo
    /// holds five. Three sits in that gap.
    ///
    /// It has to be refs and not names. The name set over-includes on
    /// purpose, one phantom per slash, so `release/2.0/rc1` adds `2.0/rc1`
    /// and `rc1` beside itself and pushes a two-ref clone to three names -
    /// over the bound, guard silent. One slash is not enough to show it:
    /// `release/2.0` gives two names against two refs, which is why a
    /// test built on it passes whichever quantity the guard reads.
    ///
    /// Three is not a tuning knob, it is the only position where the
    /// guard exists. A solo repo - one `main`, no remote - knows ONE
    /// name, which is BELOW the shallow clone's two. So any value that
    /// spares the solo repo also spares the shallow clone, and lowering
    /// to two sweeps the shallow clone anyway. There is no threshold that
    /// separates them; a solo repo with most of its review branches
    /// merged is refused, and that is the cost of the guard rather than
    /// something to calibrate away.
    const MIN_POPULATED_REFS: usize = 3;

    /// Drop the review threads and reviews of every branch that no longer
    /// exists in the repo they were filed against, and report how many
    /// branches were cleared.
    ///
    /// Every branch is judged by exact membership in one listing per
    /// project - see [`forge_agent::env::worktree::repo_branch_names`] for
    /// why a per-branch ref pattern cannot answer this. A project whose
    /// root does not answer as a work-tree root is skipped whole, as is
    /// one absent from `forge.toml`: there is nothing to check against.
    pub fn sweep_dead_review_branches(&self) -> usize {
        let mut cleared = 0;
        for view in self.list_projects() {
            let Some(repo) = forge_agent::env::worktree::repo_branch_names(&view.path) else {
                continue;
            };
            let stored = {
                let guard = self.db.lock();
                // Workspace-wide, so a closed store ends the sweep rather
                // than skipping one project - every project after this
                // would fork two git processes to reach the same answer.
                let Some(db) = guard.as_ref() else {
                    return cleared;
                };
                match crate::store::review::stored_branches(db, &view.name) {
                    Ok(stored) => stored,
                    Err(error) => {
                        tracing::warn!(
                            target: "forge_workspace::review",
                            %error,
                            project = %view.name,
                            "review-branch sweep skipped: listing stored branches failed",
                        );
                        continue;
                    }
                }
            };
            // Claude puts a worker's worktree on `worktree-<label>`, which
            // does not exist until the worktree is created. Snapshotted
            // here, so it covers workers registered up to this point and
            // not the window from here to the delete. It is not a boot
            // race either: `live_workers` is still empty this early, the
            // only production insert being downstream of this call.
            let spawning: std::collections::HashSet<String> = self
                .list_live_workers(&view.key)
                .into_iter()
                .map(|worker| format!("worktree-{}", worker.label))
                .collect();
            let dead: Vec<&String> = stored
                .iter()
                .filter(|branch| !repo.names.contains(*branch) && !spawning.contains(*branch))
                .collect();
            if dead.is_empty() {
                continue;
            }
            if dead.len() * 2 > stored.len() && repo.ref_count < Self::MIN_POPULATED_REFS {
                tracing::warn!(
                    target: "forge_workspace::review",
                    project = %view.name,
                    dead = dead.len(),
                    stored = stored.len(),
                    refs_seen = repo.ref_count,
                    "review-branch sweep refused: most of this project's stored branches read as dead against a repo that knows almost no branches, which is what a shallow or single-branch clone looks like; nothing deleted",
                );
                continue;
            }
            for branch in dead {
                let guard = self.db.lock();
                let Some(db) = guard.as_ref() else {
                    return cleared;
                };
                match crate::store::review::delete_branch_state(db, &view.name, branch) {
                    Ok(()) => {
                        tracing::info!(
                            target: "forge_workspace::review",
                            project = %view.name,
                            branch = %branch,
                            "dropped review state for a branch that no longer exists",
                        );
                        cleared += 1;
                    }
                    Err(error) => tracing::warn!(
                        target: "forge_workspace::review",
                        %error,
                        project = %view.name,
                        branch = %branch,
                        "dropping review state failed",
                    ),
                }
            }
        }
        cleared
    }

    /// Run [`Self::sweep_dead_review_branches`] off the startup path. Two
    /// `git` calls per project, both blocking, so it goes on the blocking
    /// pool rather than holding boot up.
    pub fn start_review_branch_sweep(self: &Arc<Self>) {
        let workspace = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let cleared = workspace.sweep_dead_review_branches();
            if cleared > 0 {
                tracing::info!(
                    target: "forge_workspace::review",
                    cleared,
                    "review-branch sweep cleared orphaned branches",
                );
            }
        });
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
                target: "forge_workspace::review",
                %error,
                project = %project,
                branch = %branch,
                "loading reviews failed",
            );
            format!("{error:#}")
        })
    }

    /// Worker answers on `(project, branch)` still owed a reviewer turn,
    /// paired with when the oldest of them landed. `None` when nothing
    /// waits or the row can't be read.
    ///
    /// The tally is the same `awaits_reviewer` count that
    /// `drain_review_activity` puts on a notice and that the `/diff`
    /// hydrate parks, over the same rows - so recomputing here
    /// reproduces what those two would have written rather than
    /// inventing a second number. `since` comes off the waiting
    /// threads' own `updated_at`, which for a thread awaiting the
    /// reviewer is when the agent replied, so a recompute dates the
    /// wait from the answer rather than from the recompute.
    pub fn review_replies_waiting(
        &self,
        project: &str,
        branch: &str,
    ) -> Option<(usize, std::time::SystemTime)> {
        use time::format_description::well_known::Rfc3339;

        let threads = self.load_review_threads(project, branch).ok()?;
        let waiting: Vec<_> = threads.iter().filter(|t| t.awaits_reviewer()).collect();
        if waiting.is_empty() {
            return None;
        }
        let since = waiting
            .iter()
            .filter_map(|t| time::OffsetDateTime::parse(&t.updated_at, &Rfc3339).ok())
            .min()
            .map_or_else(std::time::SystemTime::now, std::time::SystemTime::from);
        Some((waiting.len(), since))
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
                target: "forge_workspace::review",
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
                        target: "forge_workspace::review",
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
                    target: "forge_workspace::review",
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
                        target: "forge_workspace::review",
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
                        target: "forge_workspace::review",
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
                        target: "forge_workspace::review",
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
                        target: "forge_workspace::review",
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
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

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

        ws.delete_branch_review_state("forge", "feat");
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

    /// The recompute must land on the number the two live writers would
    /// have written - the `awaits_reviewer` tally over the branch's
    /// rows - and date the wait from the answer that has been sitting
    /// longest, not from the moment it was recomputed.
    #[test]
    fn review_replies_waiting_tallies_answers_and_dates_them_from_the_oldest() {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        let thread = |id: &str, status: ReviewStatus, answered_at: Option<&str>| {
            let mut comments = vec![ReviewComment {
                author: ReviewAuthor::User,
                text: "look at this".to_owned(),
                at: "2026-07-19T10:00:00Z".to_owned(),
                review_id: None,
            }];
            if let Some(at) = answered_at {
                comments.push(ReviewComment {
                    author: ReviewAuthor::Agent { label: "impl".to_owned() },
                    text: "done".to_owned(),
                    at: at.to_owned(),
                    review_id: None,
                });
            }
            ReviewThread {
                id: id.to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 1,
                    content_hash: 1,
                    context: vec!["ctx".to_owned()],
                    base_ref: "main".to_owned(),
                },
                comments,
                status,
                created_at: "2026-07-19T10:00:00Z".to_owned(),
                updated_at: answered_at.unwrap_or("2026-07-19T10:00:00Z").to_owned(),
                commit: None,
            }
        };

        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        assert!(ws.review_replies_waiting("forge", "feat").is_none(), "no rows, no signal");

        ws.save_review_threads(
            "forge",
            "feat",
            &[
                thread("answered-late", ReviewStatus::Addressed, Some("2026-07-20T12:00:00Z")),
                thread("answered-early", ReviewStatus::Addressed, Some("2026-07-19T11:00:00Z")),
                thread("unanswered", ReviewStatus::Open, None),
                thread("resolved", ReviewStatus::Resolved, Some("2026-07-20T09:00:00Z")),
            ],
        );

        let (count, since) = ws.review_replies_waiting("forge", "feat").expect("answers waiting");
        assert_eq!(count, 2, "only the answered-and-unread threads are owed a turn");
        let expected = std::time::SystemTime::from(
            time::OffsetDateTime::parse(
                "2026-07-19T11:00:00Z",
                &time::format_description::well_known::Rfc3339,
            )
            .expect("parse"),
        );
        assert_eq!(since, expected, "the wait is dated from the answer that has sat longest");

        assert!(
            ws.review_replies_waiting("forge", "other").is_none(),
            "the tally is scoped to its own branch",
        );
    }

    fn sweep_git(dir: &std::path::Path, args: &[&str]) {
        let out =
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repo at `dir` on `branch`, with one commit so HEAD resolves.
    /// `git init -b` needs git 2.28; CI runs 2.25, hence `symbolic-ref`.
    fn sweep_init_repo(dir: &std::path::Path, branch: &str) {
        sweep_git(dir, &["init", "-q"]);
        sweep_git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]);
        sweep_git(dir, &["config", "user.email", "test@example.com"]);
        sweep_git(dir, &["config", "user.name", "Test"]);
        sweep_git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "hi\n").expect("write");
        sweep_git(dir, &["add", "."]);
        sweep_git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// `origin/HEAD` as a clone leaves it: a symref, not a plain ref.
    /// The distinction is what the population count turns on.
    fn sweep_symref_head(root: &std::path::Path, target: &str) {
        sweep_git(root, &["update-ref", &format!("refs/remotes/origin/{target}"), "HEAD"]);
        sweep_git(
            root,
            &["symbolic-ref", "refs/remotes/origin/HEAD", &format!("refs/remotes/origin/{target}")],
        );
    }

    fn sweep_thread(id: &str) -> forge_primitives::review::ReviewThread {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        ReviewThread {
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
                text: "look".to_owned(),
                at: "2026-07-19T10:00:00Z".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-19T10:00:00Z".to_owned(),
            updated_at: "2026-07-19T10:00:00Z".to_owned(),
            commit: None,
        }
    }

    /// A workspace with the store open and one project rooted at `root`.
    fn sweep_ws(
        dir: &std::path::Path,
        name: &str,
        root: &std::path::Path,
    ) -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<crate::SessionUpdate>) {
        let (ws, rx) = Workspace::testing_stub_with_config_dir(dir.to_owned());
        ws.install_db_for_test(crate::store::Db::open(&dir.join("db.redb")).expect("open db"));
        ws.seed_test_project(name, &root.to_string_lossy());
        (ws, rx)
    }

    fn sweep_has_state(ws: &Arc<Workspace>, project: &str, branch: &str) -> bool {
        !ws.load_review_threads(project, branch).expect("load").is_empty()
            || !ws.load_reviews(project, branch).expect("load").is_empty()
    }

    /// Rows outlive the worker that wrote them on purpose, so the only
    /// thing that may clear them is the branch itself being gone. A
    /// remote-tracking ref counts as present - the local head is routinely
    /// deleted after a push while the PR is still open. And judgement is
    /// exact membership, not a ref pattern: `fix` and `fix/renamed` cannot
    /// coexist in git, so a stored `fix` beside a live `fix/renamed` means
    /// `fix` was deleted, where a pattern lookup would have matched its
    /// replacement and spared it forever.
    #[test]
    fn the_sweep_clears_dead_branches_and_spares_every_live_one() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("myproj");
        std::fs::create_dir_all(&root).expect("mkdir");
        sweep_init_repo(&root, "main");
        for branch in ["feat/live", "feat/second", "feat/third", "fix/renamed"] {
            sweep_git(&root, &["branch", branch]);
        }
        sweep_git(&root, &["update-ref", "refs/remotes/origin/feat/pushed", "HEAD"]);

        let (ws, _rx) = sweep_ws(dir.path(), "myproj", &root);
        let reviewer = SessionKey::from_session_id("lead-uuid");
        // Four live to three dead, so the pass stays under the refusal
        // bound - a sweep is meant to be a trickle, and the ratio that
        // trips the bound is asserted separately.
        for branch in
            ["feat/live", "feat/second", "feat/third", "feat/pushed", "feat/merged", "fix"]
        {
            ws.save_review_threads("myproj", branch, &[sweep_thread("a")]);
            ws.submit_review("myproj", branch, None, &["a".to_owned()], reviewer.clone())
                .expect("submit");
        }
        // Threads drafted but never submitted: no reviews row at all.
        ws.save_review_threads("myproj", "feat/drafts", &[sweep_thread("d")]);

        assert_eq!(ws.sweep_dead_review_branches(), 3, "feat/merged, fix and feat/drafts");

        assert!(sweep_has_state(&ws, "myproj", "feat/live"), "a local head survives");
        assert!(sweep_has_state(&ws, "myproj", "feat/second"), "so do the other live ones");
        assert!(sweep_has_state(&ws, "myproj", "feat/third"), "so do the other live ones");
        assert!(sweep_has_state(&ws, "myproj", "feat/pushed"), "a remote-tracking ref survives");
        assert!(!sweep_has_state(&ws, "myproj", "feat/merged"), "no ref anywhere, cleared");
        assert!(!sweep_has_state(&ws, "myproj", "fix"), "fix/renamed is not fix");
        assert!(!sweep_has_state(&ws, "myproj", "feat/drafts"), "drafts go with their branch");
    }

    /// A store the sweep cannot check against is left alone. With no repo
    /// there is nothing to be absent from, and reading that as absence
    /// would clear every row the project has.
    #[test]
    fn the_sweep_spares_a_project_it_cannot_check() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("plainproj");
        std::fs::create_dir_all(&root).expect("mkdir");

        let (ws, _rx) = sweep_ws(dir.path(), "plainproj", &root);
        ws.save_review_threads("plainproj", "feat/x", &[sweep_thread("a")]);

        assert_eq!(ws.sweep_dead_review_branches(), 0, "nothing to verify, nothing to clear");
        assert!(sweep_has_state(&ws, "plainproj", "feat/x"));
    }

    /// The case no per-branch check can catch: git succeeds, the path is
    /// right, and the branches really are absent from the ref set it can
    /// see. A shallow or `--single-branch` clone knows two names - its own
    /// branch, and `HEAD` via `origin/HEAD` - so the ref count is what
    /// tells it apart from a large but genuine cleanup.
    #[test]
    fn the_sweep_refuses_a_repo_that_knows_almost_no_branches() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("recloned");
        std::fs::create_dir_all(&root).expect("mkdir");
        sweep_init_repo(&root, "main");
        sweep_symref_head(&root, "main");

        let (ws, _rx) = sweep_ws(dir.path(), "recloned", &root);
        for branch in ["feat/a", "feat/b", "feat/c"] {
            ws.save_review_threads("recloned", branch, &[sweep_thread("a")]);
        }

        assert_eq!(ws.sweep_dead_review_branches(), 0, "refused, not swept");
        for branch in ["feat/a", "feat/b", "feat/c"] {
            assert!(sweep_has_state(&ws, "recloned", branch), "{branch} kept");
        }
    }

    /// The accumulation #598 exists to clear IS the dead set, so a bound
    /// keyed on the ratio alone would refuse hardest on the store it was
    /// written for. A repo full of refs is a real cleanup however lopsided
    /// the ratio.
    #[test]
    fn the_sweep_clears_a_long_backlog_when_the_repo_is_well_populated() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("busy");
        std::fs::create_dir_all(&root).expect("mkdir");
        sweep_init_repo(&root, "main");
        for branch in ["feat/live", "feat/second", "feat/third"] {
            sweep_git(&root, &["branch", branch]);
        }

        let (ws, _rx) = sweep_ws(dir.path(), "busy", &root);
        // Eight long-merged branches against one live one: the ratio is
        // lopsided, the repo is not.
        for n in 0..8 {
            ws.save_review_threads("busy", &format!("feat/merged-{n}"), &[sweep_thread("a")]);
        }
        ws.save_review_threads("busy", "feat/live", &[sweep_thread("b")]);

        assert_eq!(ws.sweep_dead_review_branches(), 8, "the backlog is what this is for");
        assert!(sweep_has_state(&ws, "busy", "feat/live"));
    }

    /// Both halves are required. A repo that knows few branches is only
    /// refused when most of the store reads dead against it - one merged
    /// branch beside a live one is an ordinary tidy-up, and a small local
    /// repo is still allowed to have it swept.
    #[test]
    fn a_sparse_repo_still_sweeps_when_only_a_minority_reads_dead() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("small");
        std::fs::create_dir_all(&root).expect("mkdir");
        sweep_init_repo(&root, "main");
        sweep_symref_head(&root, "main");

        let (ws, _rx) = sweep_ws(dir.path(), "small", &root);
        ws.save_review_threads("small", "main", &[sweep_thread("a")]);
        ws.save_review_threads("small", "feat/merged", &[sweep_thread("b")]);

        assert_eq!(ws.sweep_dead_review_branches(), 1, "one of two dead is not a majority");
        assert!(sweep_has_state(&ws, "small", "main"), "the live branch keeps its state");
        assert!(!sweep_has_state(&ws, "small", "feat/merged"));
    }

    /// Where the threshold sits, and that a symref is not counted toward
    /// it. Both poles use local heads only, so `names` and `ref_count`
    /// coincide here - this pins the position, NOT which of the two the
    /// guard reads; the slash test below is the only thing that does
    /// that.
    #[test]
    fn the_refusal_threshold_sits_between_two_and_three_branch_refs() {
        let scenario = |name: &str, extra_branches: usize| {
            let dir = tempdir().expect("tempdir");
            let root = dir.path().join(name);
            std::fs::create_dir_all(&root).expect("mkdir");
            sweep_init_repo(&root, "main");
            sweep_symref_head(&root, "main");
            for n in 0..extra_branches {
                sweep_git(&root, &["branch", &format!("feat/keep-{n}")]);
            }
            let (ws, _rx) = sweep_ws(dir.path(), name, &root);
            for branch in ["feat/a", "feat/b"] {
                ws.save_review_threads(name, branch, &[sweep_thread("a")]);
            }
            (dir, ws.sweep_dead_review_branches())
        };
        // The clone shape alone is two refs - the local head and the one
        // remote-tracking branch its symref points at. Under the bound.
        let (_d1, under) = scenario("bare", 0);
        assert_eq!(under, 0, "two branch refs is not enough to trust a majority-dead pass");
        // One more local head makes three, and the same ratio goes through.
        let (_d2, over) = scenario("populated", 1);
        assert_eq!(over, 2, "at three branch refs the same ratio sweeps");
    }

    /// The guard has to survive a rename. Each slash in a branch name
    /// adds a phantom suffix to the over-including name set, so
    /// `release/2.0/rc1` contributes `2.0/rc1` and `rc1` on top of
    /// itself: a guard reading that set as a population sees three where
    /// the clone holds two refs, stops firing, and deletes. The count is
    /// of refs and does not move when a branch is renamed.
    ///
    /// Two slash levels, not one - at one the name set and the ref count
    /// are both two, and the test passes whichever quantity the guard
    /// reads.
    #[test]
    fn a_slash_in_a_branch_name_does_not_switch_the_refusal_off() {
        let shallow_clone_shaped = |name: &str, default: &str| {
            let dir = tempdir().expect("tempdir");
            let root = dir.path().join(name);
            std::fs::create_dir_all(&root).expect("mkdir");
            sweep_init_repo(&root, default);
            // What `git clone --depth 1` leaves: the local head, one
            // remote-tracking ref, and origin/HEAD.
            sweep_git(&root, &["update-ref", &format!("refs/remotes/origin/{default}"), "HEAD"]);
            sweep_symref_head(&root, default);
            let (ws, _rx) = sweep_ws(dir.path(), name, &root);
            for branch in ["feat/a", "feat/b"] {
                ws.save_review_threads(name, branch, &[sweep_thread("a")]);
            }
            (dir, ws.sweep_dead_review_branches())
        };
        let (_d1, plain) = shallow_clone_shaped("plain", "main");
        assert_eq!(plain, 0, "the shallow clone is refused");
        let (_d2, slashed) = shallow_clone_shaped("slashed", "release/2.0/rc1");
        assert_eq!(slashed, 0, "and renaming its default branch does not turn the guard off");
    }

    /// Auto-start and the kick dispatcher are creating worker worktrees at
    /// the moment the sweep runs, so a registered worker's branch being
    /// absent right now is routine rather than evidence it is gone.
    #[test]
    fn the_sweep_spares_a_branch_a_live_worker_has_yet_to_create() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("myproj");
        std::fs::create_dir_all(&root).expect("mkdir");
        sweep_init_repo(&root, "main");

        let (ws, _rx) = sweep_ws(dir.path(), "myproj", &root);
        let project_key =
            ws.list_projects().into_iter().find(|v| v.name == "myproj").expect("project").key;
        ws.insert_live_worker(
            &project_key,
            crate::WorkerEntry {
                label: "reviewer".to_owned(),
                charter: "review".to_owned(),
                session_key: SessionKey::from_session_id("worker-uuid"),
                status: forge_primitives::WorkerLiveness::Spawning,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".to_owned(),
                needs_tag: false,
                is_git_repo_at_spawn: true,
                diagnostic: None,
                kick: None,
            },
        );
        ws.save_review_threads("myproj", "worktree-reviewer", &[sweep_thread("a")]);

        assert_eq!(ws.sweep_dead_review_branches(), 0, "its worktree is still being created");
        assert!(sweep_has_state(&ws, "myproj", "worktree-reviewer"));
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
}
