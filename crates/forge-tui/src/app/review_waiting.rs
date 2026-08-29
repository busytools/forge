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
//! the subprocess off the render thread. The in-flight guard is that
//! module's too, and for the same reason - a task that dies without
//! sending must not strand the session.
//!
//! One deliberate difference from the notice writer, which routes to the
//! submit-origin session alone and drops rather than mis-route: this
//! addresses every session whose checkout is on the branch. At boot there
//! is no origin to route by - the map died with the process - so there is
//! no principled single target, and two sessions sharing a checkout are
//! both genuinely on the branch the count is about. It parks the same
//! value on both rather than picking one arbitrarily.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;

use forge_primitives::review::ReviewThread;
use forge_workspace::SessionKey;

use crate::app::App;

/// Max events applied per drain pump tick, matching the budget the
/// sibling drains use.
const EVENT_DRAIN_BUDGET: usize = 64;

/// Consecutive failed reads after which a session stops being retried.
///
/// A read that fails is not an answer, and it is not always transient:
/// a non-git project resolves to `NotARepo`, while a timeout, a
/// checkout git refuses and an unrunnable git all resolve to
/// `ScannerFailed`. Every one of them is an `Err`, so without a bound
/// each would spawn one `git` per second for the life of the process -
/// and each one logs, so it would also evict the diagnostic log the
/// self-serve rule depends on. Three lets a transient timeout retry
/// twice and stops anything permanent.
const MAX_FAILED_READS: u8 = 3;

/// A finished recompute.
#[derive(Debug)]
pub struct ReviewWaitingEvent {
    pub key: SessionKey,
    pub outcome: ReviewWaitingOutcome,
}

#[derive(Debug)]
pub enum ReviewWaitingOutcome {
    /// The recompute reached an answer. `None` means nothing is owed -
    /// still an answer, and still what retires the session.
    Answered(Option<crate::app::ReviewRepliesWaiting>),
    /// git could not be read for this checkout.
    Unreadable,
}

/// Clears the in-flight flag when the task ends, however it ends. Mirrors
/// `git_diff::ScanInFlightGuard`: without it a panic or a runtime abort
/// mid-`git` leaves the flag set and the session never retries.
struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Kick the recompute for every connected session that has not settled
/// one and has none in flight.
///
/// Gated on a resolved `session_id` because the worker registry is
/// written before `Connected` fires; running earlier could resolve a
/// worker's scan dir to the project root and count the lead's branch
/// against the worker's session.
pub fn hydrate_pending(app: &mut App) {
    let Some(workspace) = app.workspace.clone() else {
        return;
    };
    let pending: Vec<(SessionKey, String, PathBuf, Arc<AtomicBool>)> = app
        .sessions
        .iter()
        .filter(|(_, session)| {
            !session.review_waiting_settled
                && !session.review_waiting_in_flight.load(Ordering::Acquire)
                && session.session_id.is_some()
                && !session.cwd_raw.is_empty()
        })
        .filter_map(|(key, session)| {
            Some((
                key.clone(),
                session.project.clone()?,
                PathBuf::from(&session.cwd_raw),
                Arc::clone(&session.review_waiting_in_flight),
            ))
        })
        .collect();
    for (key, project, cwd_raw, in_flight) in pending {
        // Compare-and-swap so two callers in one tick can't both win.
        if in_flight.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            continue;
        }
        let cwd = workspace.git_scan_cwd_for_session(&key, &cwd_raw);
        request_refresh(
            app.review_waiting_event_tx.clone(),
            key,
            project,
            cwd,
            Arc::clone(&workspace),
            in_flight,
        );
    }
}

/// Resolve `cwd`'s branch, recompute that `(project, branch)`'s waiting
/// count, and post the outcome back. A detached HEAD answers "no branch
/// to key by", which is a fact rather than a failure; anything git could
/// not read is [`ReviewWaitingOutcome::Unreadable`] and leaves the retry
/// decision to the drain, which counts them.
fn request_refresh(
    tx: std_mpsc::Sender<ReviewWaitingEvent>,
    key: SessionKey,
    project: String,
    cwd: PathBuf,
    workspace: Arc<forge_workspace::Workspace>,
    in_flight: Arc<AtomicBool>,
) {
    let guard = InFlightGuard(in_flight);
    tokio::task::spawn_local(async move {
        // Moved in so its lifetime brackets the await.
        let _guard = guard;
        let Ok(branch) = forge_workspace::env::git_diff::current_branch(&cwd).await else {
            let _ = tx.send(ReviewWaitingEvent { key, outcome: ReviewWaitingOutcome::Unreadable });
            return;
        };
        let waiting = branch.and_then(|branch| {
            workspace
                .review_replies_waiting(&project, &branch)
                .map(|(count, since)| crate::app::ReviewRepliesWaiting { branch, count, since })
        });
        let _ =
            tx.send(ReviewWaitingEvent { key, outcome: ReviewWaitingOutcome::Answered(waiting) });
    });
}

/// Drain finished recomputes onto their session buckets. Called from the
/// main loop alongside the other drain pumps.
pub fn drain_events(app: &mut App) {
    let workspace = app.workspace.clone();
    for _ in 0..EVENT_DRAIN_BUDGET {
        let Ok(event) = app.review_waiting_event_rx.try_recv() else {
            return;
        };
        // Retire a parked signal whose own branch is owed nothing now -
        // the recompute below only answers for the branch the checkout
        // is on, and every other writer leaves other branches alone.
        //
        // Reading the threads rather than the tally, because the tally
        // reports a read failure as `None` too, and a transient one must
        // not retire a live signal.
        let parked = app.sessions.get(&event.key).and_then(|s| {
            Some((s.review_replies_waiting.as_ref()?.branch.clone(), s.project.clone()?))
        });
        if let Some((branch, project)) = parked
            && let Some(ws) = workspace.as_ref()
            && ws
                .load_review_threads(&project, &branch)
                .is_ok_and(|threads| !threads.iter().any(ReviewThread::awaits_reviewer))
            && let Some(session) = app.sessions.get_mut(&event.key)
        {
            session.review_replies_waiting = None;
            app.needs_redraw = true;
        }
        let Some(session) = app.sessions.get_mut(&event.key) else {
            continue;
        };
        let waiting = match event.outcome {
            ReviewWaitingOutcome::Answered(waiting) => {
                session.review_waiting_settled = true;
                waiting
            }
            ReviewWaitingOutcome::Unreadable => {
                session.review_waiting_failed_reads =
                    session.review_waiting_failed_reads.saturating_add(1);
                if session.review_waiting_failed_reads >= MAX_FAILED_READS {
                    // Retried enough. Whatever this checkout is, it is not
                    // going to answer, and each attempt costs a `git` and
                    // a log line.
                    session.review_waiting_settled = true;
                    tracing::debug!(
                        target: crate::logging::targets::APP_SESSION,
                        event_name = "review_waiting_gave_up",
                        message = "git could not be read for this checkout; giving up on restoring its review-replies count",
                        outcome = "skipped",
                        key = %event.key.as_str(),
                        attempts = session.review_waiting_failed_reads,
                    );
                }
                continue;
            }
        };
        let Some(waiting) = waiting else {
            continue;
        };
        // A signal already on the bucket outranks this one: both live
        // writers compute the same tally from the same rows, and either
        // landed after this recompute was kicked off.
        if session.review_replies_waiting.is_some() {
            continue;
        }
        session.review_replies_waiting = Some(waiting);
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

    /// A booted App holding one connected session rooted at `repo`, with
    /// the review store open. Mirrors the state a restart leaves: the
    /// bucket is there, nothing has opened `/diff` on it.
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

    /// Drive the spawned recompute to completion and apply it. Loops on
    /// the state it is waiting for, with the count only as a cap - the
    /// spawned `git` runs against a 10s timeout, so any fixed budget
    /// would be asserting on how loaded the runner is.
    async fn settle(app: &mut App, key: &SessionKey) {
        for _ in 0..1500 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            drain_events(app);
            if app.sessions.get(key).is_some_and(|s| s.review_waiting_settled) {
                return;
            }
        }
        panic!("recompute did not settle");
    }

    /// Same, for a pass that ends without settling: wait until the slot
    /// is released and the event is drained.
    async fn drain_until_idle(app: &mut App, key: &SessionKey) {
        for _ in 0..1500 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            drain_events(app);
            let idle = app
                .sessions
                .get(key)
                .is_some_and(|s| !s.review_waiting_in_flight.load(Ordering::Acquire));
            if idle {
                return;
            }
        }
        panic!("recompute never released its slot");
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
                settle(&mut app, &key).await;
            })
            .await;

        let restored = waiting(&app, &key).expect("the count is back");
        assert_eq!(restored.count, 2, "both answers are still owed a reviewer turn");
        assert_eq!(restored.branch, "feat/worker", "keyed on the branch the checkout is on");
    }

    /// The checkout has moved to `main`, so the recompute asks about
    /// `main` and nothing asks about the branch the parked signal belongs
    /// to - which is how a branch that was resolved, merged and deleted
    /// keeps the band lit.
    #[tokio::test(flavor = "current_thread")]
    async fn a_parked_count_for_a_branch_the_checkout_has_left_clears() {
        let repo = tempfile::tempdir().expect("tempdir");
        let db = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "main");
        let (mut app, key) = booted_app(repo.path(), db.path());
        // The store holds nothing for feat/worker: its threads were
        // resolved, or the boot sweep dropped the dead branch's rows.
        app.sessions.get_mut(&key).expect("session").review_replies_waiting =
            crate::app::ReviewRepliesWaiting::merge(None, "feat/worker", 2);

        tokio::task::LocalSet::new()
            .run_until(async {
                hydrate_pending(&mut app);
                settle(&mut app, &key).await;
            })
            .await;

        assert_eq!(
            waiting(&app, &key),
            None,
            "nothing is owed on that branch any more, so the band must stop saying so",
        );
    }

    /// A redb read failure is not an answer. `review_replies_waiting`
    /// reports `None` for a read error exactly as it does for "nothing
    /// owed", so the two have to be told apart before anything is retired.
    #[tokio::test(flavor = "current_thread")]
    async fn a_read_failure_does_not_retire_a_live_signal() {
        let repo = tempfile::tempdir().expect("tempdir");
        let db_dir = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "main");
        let db = forge_workspace::store::Db::open(&db_dir.path().join("db.redb")).expect("db");
        forge_workspace::store::review::write_corrupt_row_for_test(&db, "forge", "feat/worker")
            .expect("corrupt row");

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.install_db_for_test(db);
        let key = SessionKey::from_session_id("restored-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = repo.path().to_string_lossy().into_owned();
        session.session_id = Some(crate::agent::model::SessionId::new("restored-session"));
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key.clone());
        app.sessions.get_mut(&key).expect("session").review_replies_waiting =
            crate::app::ReviewRepliesWaiting::merge(None, "feat/worker", 2);

        tokio::task::LocalSet::new()
            .run_until(async {
                hydrate_pending(&mut app);
                settle(&mut app, &key).await;
            })
            .await;

        assert_eq!(
            waiting(&app, &key).map(|w| w.count),
            Some(2),
            "the store could not answer, so the signal stands rather than being retired",
        );
    }

    /// The cross-branch rule this must not trample: a live count on
    /// another branch is real work, and the checkout being elsewhere
    /// says nothing about it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_signal_for_another_branch_that_is_still_owed_survives() {
        let repo = tempfile::tempdir().expect("tempdir");
        let db = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "main");
        let (mut app, key) = booted_app(repo.path(), db.path());
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat/worker", &[answered_thread("a")]);
        app.sessions.get_mut(&key).expect("session").review_replies_waiting =
            crate::app::ReviewRepliesWaiting::merge(None, "feat/worker", 1);

        tokio::task::LocalSet::new()
            .run_until(async {
                hydrate_pending(&mut app);
                settle(&mut app, &key).await;
            })
            .await;

        let kept = waiting(&app, &key).expect("the other branch still owes a turn");
        assert_eq!(kept.branch, "feat/worker");
        assert_eq!(kept.count, 1, "being on main does not retire work on feat/worker");
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
                settle(&mut app, &key).await;
            })
            .await;

        assert_eq!(
            waiting(&app, &key).expect("signal").count,
            1,
            "the live signal stands; the recompute does not overwrite it",
        );
    }

    /// One in flight at a time, and spawning is not settling. The ~1s
    /// ticker calls this on every pass, so without the flag each session
    /// would spawn a fresh `git` call a second while the first still ran.
    /// Asserted on the flags, which move synchronously - a wall-clock
    /// budget for the subprocess would just be a slow way to fail on a
    /// loaded runner.
    #[test]
    fn a_session_with_a_recompute_in_flight_is_not_queued_again() {
        let repo = tempfile::tempdir().expect("tempdir");
        let db = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "feat/worker");
        let (mut app, key) = booted_app(repo.path(), db.path());

        let runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(tokio::task::LocalSet::new().run_until(async {
            hydrate_pending(&mut app);
            let in_flight =
                Arc::clone(&app.sessions.get(&key).expect("session").review_waiting_in_flight);
            assert!(in_flight.load(Ordering::Acquire), "the first pass claims the slot");
            assert!(
                !app.sessions.get(&key).expect("session").review_waiting_settled,
                "spawning is not settling - only an answer retires the session",
            );
            // Release the slot as the guard would, then confirm a settled
            // session is not re-queued either.
            in_flight.store(false, Ordering::Release);
            app.sessions.get_mut(&key).expect("session").review_waiting_settled = true;
            hydrate_pending(&mut app);
            assert!(!in_flight.load(Ordering::Acquire), "a settled session is never queued again");
        }));
    }

    /// A read that failed is not an answer, and it is not always
    /// transient: a path that is not on disk resolves to `NotARepo` and
    /// a real timeout to `ScannerFailed`, both of them `Err`. So retries
    /// are counted and bounded - an unreadable checkout must not spawn
    /// one `git` per second, per session, for the life of the process.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unreadable_checkout_retries_a_bounded_number_of_times() {
        let db = tempfile::tempdir().expect("tempdir");
        let gone = db.path().join("not-on-disk");
        let (mut app, key) = booted_app(&gone, db.path());

        tokio::task::LocalSet::new()
            .run_until(async {
                for attempt in 1..=u32::from(MAX_FAILED_READS) {
                    hydrate_pending(&mut app);
                    drain_until_idle(&mut app, &key).await;
                    let session = app.sessions.get(&key).expect("session");
                    assert_eq!(
                        u32::from(session.review_waiting_failed_reads),
                        attempt,
                        "each pass counts one failed read",
                    );
                }
                // The cap is reached, so the session retires and further
                // passes queue nothing at all.
                assert!(
                    app.sessions.get(&key).expect("session").review_waiting_settled,
                    "a checkout that will not answer stops being retried",
                );
                hydrate_pending(&mut app);
                assert!(
                    !app.sessions
                        .get(&key)
                        .expect("session")
                        .review_waiting_in_flight
                        .load(Ordering::Acquire),
                    "nothing is queued once the session has given up",
                );
                assert_eq!(
                    app.sessions.get(&key).expect("session").review_waiting_failed_reads,
                    MAX_FAILED_READS,
                    "and the count stops climbing",
                );
            })
            .await;
    }

    /// The opposite case, and why a bare "did it send" flag would not do:
    /// a session with nothing waiting has reached a real answer and must
    /// retire, or the ticker re-runs `git` for it every second forever.
    #[tokio::test(flavor = "current_thread")]
    async fn a_session_with_nothing_waiting_still_settles() {
        let repo = tempfile::tempdir().expect("tempdir");
        let db = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "feat/worker");
        let (mut app, key) = booted_app(repo.path(), db.path());

        tokio::task::LocalSet::new()
            .run_until(async {
                hydrate_pending(&mut app);
                settle(&mut app, &key).await;
            })
            .await;

        let session = app.sessions.get(&key).expect("session");
        assert!(session.review_waiting_settled, "an empty store is an answer");
        assert!(session.review_replies_waiting.is_none(), "and nothing is parked");
    }
}
