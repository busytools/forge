//! One-shot rehydration of a session's review-replies-waiting signal.
//!
//! The signal is fed live two ways - `SessionUpdate::ReviewActivityNotice`
//! when a worker's review turn ends, and an authoritative recompute
//! whenever `/diff` hydrates its threads - and neither survives a
//! restart: the notice's submit-origin map is in-memory only, and the
//! recompute needs the overlay open. So a freshly booted forge shows
//! nothing until the reviewer opens `/diff` on that branch, which is the
//! moment they least need reminding.
//!
//! This fills that window by recomputing each session's count from the
//! store once, off the `git_scan_cwd_for_session` + `current_branch`
//! derivation the review MCP's reader uses. The tally is the same
//! `awaits_reviewer` count over the same rows that both live writers
//! produce, so what lands here is the number they would have written.
//!
//! Same shape as [`crate::app::git_diff`]: a spawned local task runs the
//! `git` call and hands the result back over a std mpsc channel, keeping
//! the subprocess off the render thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::SystemTime;

use forge_workspace::SessionKey;

use crate::app::App;

/// Max events applied per drain pump tick, matching the budget the
/// sibling drains use.
const EVENT_DRAIN_BUDGET: usize = 64;

/// A completed recompute. Only ever sent for a non-zero count: a
/// session with nothing waiting has nothing to park, and staying silent
/// keeps the common case off the redraw path.
#[derive(Debug)]
pub struct ReviewWaitingEvent {
    pub key: SessionKey,
    pub branch: String,
    pub count: usize,
    pub since: SystemTime,
}

/// Kick the recompute for every connected session that has not had one,
/// marking each as done before it spawns so the ~1s ticker can't queue a
/// second `git` call for the same session.
///
/// Gated on a resolved `session_id` because the worker registry is
/// written before `Connected` fires; running earlier could resolve a
/// worker's scan dir to the project root and count the lead's branch
/// against the worker's session.
pub fn hydrate_pending(app: &mut App) {
    let Some(workspace) = app.workspace.clone() else {
        return;
    };
    let pending: Vec<(SessionKey, String, PathBuf)> = app
        .sessions
        .iter()
        .filter(|(_, session)| {
            !session.review_waiting_hydrated
                && session.session_id.is_some()
                && !session.cwd_raw.is_empty()
        })
        .filter_map(|(key, session)| {
            Some((key.clone(), session.project.clone()?, PathBuf::from(&session.cwd_raw)))
        })
        .collect();
    for (key, project, cwd_raw) in pending {
        if let Some(session) = app.sessions.get_mut(&key) {
            session.review_waiting_hydrated = true;
        }
        let cwd = workspace.git_scan_cwd_for_session(&key, &cwd_raw);
        request_refresh(
            app.review_waiting_event_tx.clone(),
            key,
            project,
            cwd,
            Arc::clone(&workspace),
        );
    }
}

/// Resolve `cwd`'s branch and recompute that `(project, branch)`'s
/// waiting count, posting it back when something is waiting. Every
/// failure path is silent: a detached HEAD, a non-repo cwd and an
/// unreadable row all mean "no signal to restore", and `current_branch`
/// logs git's own error where it happens.
fn request_refresh(
    tx: std_mpsc::Sender<ReviewWaitingEvent>,
    key: SessionKey,
    project: String,
    cwd: PathBuf,
    workspace: Arc<forge_workspace::Workspace>,
) {
    tokio::task::spawn_local(async move {
        let Ok(Some(branch)) = forge_workspace::env::git_diff::current_branch(&cwd).await else {
            return;
        };
        let Some((count, since)) = workspace.review_replies_waiting(&project, &branch) else {
            return;
        };
        let _ = tx.send(ReviewWaitingEvent { key, branch, count, since });
    });
}

/// Drain completed recomputes onto their session buckets. Called from
/// the main loop alongside the other drain pumps.
pub fn drain_events(app: &mut App) {
    for _ in 0..EVENT_DRAIN_BUDGET {
        let Ok(event) = app.review_waiting_event_rx.try_recv() else {
            return;
        };
        let Some(session) = app.sessions.get_mut(&event.key) else {
            continue;
        };
        // A signal already on the bucket outranks this one: both live
        // writers compute the same tally from the same rows, and either
        // landed after this recompute was kicked off.
        if session.review_replies_waiting.is_some() {
            continue;
        }
        session.review_replies_waiting = Some(crate::app::ReviewRepliesWaiting {
            branch: event.branch,
            count: event.count,
            since: event.since,
        });
        app.needs_redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::review::{
        ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
    };
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let out =
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repo at `dir` on `branch`, with one commit so HEAD resolves.
    /// `git init -b` needs git 2.28; CI runs 2.25, hence `symbolic-ref`.
    fn init_repo(dir: &Path, branch: &str) {
        git(dir, &["init", "-q"]);
        git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "hi\n").expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// A thread a worker answered and nobody has come back to.
    fn answered_thread(id: &str) -> ReviewThread {
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
            comments: vec![
                ReviewComment {
                    author: ReviewAuthor::User,
                    text: "why?".to_owned(),
                    at: "2026-07-19T10:00:00Z".to_owned(),
                    review_id: None,
                },
                ReviewComment {
                    author: ReviewAuthor::Agent { label: "impl".to_owned() },
                    text: "because".to_owned(),
                    at: "2026-07-19T11:00:00Z".to_owned(),
                    review_id: None,
                },
            ],
            status: ReviewStatus::Addressed,
            created_at: "2026-07-19T10:00:00Z".to_owned(),
            updated_at: "2026-07-19T11:00:00Z".to_owned(),
            commit: None,
        }
    }

    /// A booted App holding one connected session rooted at `repo`,
    /// with the review store open. Mirrors the state a restart leaves:
    /// the bucket is there, nothing has opened `/diff` on it.
    fn booted_app(repo: &Path, db_dir: &Path) -> (App, SessionKey) {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.install_db_for_test(
            forge_workspace::store::Db::open(&db_dir.join("db.redb")).expect("open db"),
        );
        let key = SessionKey::from_session_id("restored-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = repo.to_string_lossy().into_owned();
        session.session_id = Some(crate::agent::model::SessionId::new("restored-session"));
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key.clone());
        (app, key)
    }

    /// Drive the spawned recompute to completion and apply it.
    async fn settle(app: &mut App) {
        for _ in 0..300 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            drain_events(app);
            if app.sessions.values().any(|s| s.review_replies_waiting.is_some()) {
                break;
            }
        }
    }

    fn waiting(app: &App, key: &SessionKey) -> Option<crate::app::ReviewRepliesWaiting> {
        app.sessions.get(key).and_then(|s| s.review_replies_waiting.clone())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_restored_session_gets_its_waiting_count_back_without_opening_diff() {
        let repo = tempfile::tempdir().expect("tempdir");
        let db = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "feat/worker");
        let (mut app, key) = booted_app(repo.path(), db.path());
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat/worker",
            &[answered_thread("a"), answered_thread("b")],
        );

        tokio::task::LocalSet::new()
            .run_until(async {
                hydrate_pending(&mut app);
                settle(&mut app).await;
            })
            .await;

        let restored = waiting(&app, &key).expect("the count is back");
        assert_eq!(restored.count, 2, "both answers are still owed a reviewer turn");
        assert_eq!(restored.branch, "feat/worker", "keyed on the branch the checkout is on");
    }

    /// The recompute only fills a gap. A count that arrived while it was
    /// in flight - a worker's turn ending, or a `/diff` hydrate - is
    /// newer, and must not be rolled back to the boot-time reading.
    #[tokio::test(flavor = "current_thread")]
    async fn a_signal_that_landed_first_is_not_clobbered() {
        let repo = tempfile::tempdir().expect("tempdir");
        let db = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "feat/worker");
        let (mut app, key) = booted_app(repo.path(), db.path());
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat/worker",
            &[answered_thread("a"), answered_thread("b")],
        );

        tokio::task::LocalSet::new()
            .run_until(async {
                hydrate_pending(&mut app);
                app.sessions.get_mut(&key).expect("session").review_replies_waiting =
                    crate::app::ReviewRepliesWaiting::merge(None, "feat/worker", 1);
                settle(&mut app).await;
            })
            .await;

        assert_eq!(
            waiting(&app, &key).expect("signal").count,
            1,
            "the live signal stands; the recompute does not overwrite it",
        );
    }

    /// One shot per session. The ~1s ticker calls this on every pass, so
    /// without the marker each session would spawn a fresh `git` call a
    /// second, forever.
    #[tokio::test(flavor = "current_thread")]
    async fn a_session_is_only_ever_queued_once() {
        let repo = tempfile::tempdir().expect("tempdir");
        let db = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "feat/worker");
        let (mut app, _key) = booted_app(repo.path(), db.path());
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat/worker", &[answered_thread("a")]);

        let mut events = 0;
        tokio::task::LocalSet::new()
            .run_until(async {
                hydrate_pending(&mut app);
                hydrate_pending(&mut app);
                for _ in 0..60 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    while app.review_waiting_event_rx.try_recv().is_ok() {
                        events += 1;
                    }
                }
            })
            .await;

        assert_eq!(events, 1, "the second pass finds the session already done and spawns nothing");
    }
}
