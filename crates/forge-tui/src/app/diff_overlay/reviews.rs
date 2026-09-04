//! The review the overlay files: the `l` REVIEWS list, the
//! Finish-review modal, and the close path that seals this session's
//! authored comments into a numbered review and nudges the agent.

use super::keys::after_nav;
use super::state::{ReviewListRow, ReviewListTotals};
use super::types::{DiffScope, FinishReviewState};
use crate::app::App;
use crate::app::input::InputState;
use crossterm::event::{KeyCode, KeyEvent};
use forge_primitives::review::{ReviewStatus, ReviewThread};

/// Parse an rfc3339 timestamp into a `SystemTime`, or `None` when it is
/// empty / malformed.
pub(super) fn parse_rfc3339(text: &str) -> Option<std::time::SystemTime> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(text, &Rfc3339).ok().map(std::time::SystemTime::from)
}

/// Build the reviews-list rows from the branch's reviews plus every
/// thread's current state, newest review first. Each row tallies the
/// review's member threads (those with a turn filed into it) by status and
/// records the first member's scope + path for navigation. A thread the
/// reviewer replied on across rounds is a member of each of them.
pub(super) fn compute_review_rows(
    reviews: &[forge_primitives::ReviewSet],
    threads: &[ReviewThread],
    now: std::time::SystemTime,
) -> Vec<ReviewListRow> {
    reviews
        .iter()
        .rev()
        .map(|review| {
            let members: Vec<&ReviewThread> =
                threads.iter().filter(|t| t.is_in_review(&review.id)).collect();
            let mut open = 0;
            let mut addressed = 0;
            let mut resolved = 0;
            let mut outdated = 0;
            for t in &members {
                match t.status {
                    ReviewStatus::Open => open += 1,
                    ReviewStatus::Addressed => addressed += 1,
                    ReviewStatus::Resolved => resolved += 1,
                    ReviewStatus::Outdated => outdated += 1,
                }
            }
            let age = parse_rfc3339(&review.created_at)
                .map(|at| crate::ui::format::relative_time(at, now))
                .unwrap_or_default();
            let first = members.first();
            ReviewListRow {
                number: review.number,
                age,
                total: members.len(),
                open,
                addressed,
                resolved,
                outdated,
                summary: review.summary.clone().filter(|s| !s.trim().is_empty()),
                first_commit: first.and_then(|t| t.commit.clone()),
                first_path: first.map(|t| t.anchor.path.clone()),
            }
        })
        .collect()
}

/// Tally the branch's filed comments for the reviews-list footer, counting
/// a thread once however many reviews its turns span.
pub(super) fn compute_review_totals(
    reviews: &[forge_primitives::ReviewSet],
    threads: &[ReviewThread],
) -> ReviewListTotals {
    let mut totals = ReviewListTotals::default();
    for thread in threads.iter().filter(|t| reviews.iter().any(|r| t.is_in_review(&r.id))) {
        totals.comments += 1;
        match thread.status {
            ReviewStatus::Open => totals.open += 1,
            ReviewStatus::Addressed => totals.addressed += 1,
            ReviewStatus::Resolved => totals.resolved += 1,
            ReviewStatus::Outdated => totals.outdated += 1,
        }
    }
    totals
}

/// Toggle the `l` REVIEWS list. Opening snapshots every thread's current
/// state into per-review rollups (newest first); closing drops the rows.
pub(super) fn toggle_reviews_list(app: &mut App) {
    if app.diff_overlay.as_ref().is_some_and(|o| o.reviews_open) {
        if let Some(o) = app.diff_overlay.as_mut() {
            o.reviews_open = false;
            o.review_rows.clear();
            o.review_totals = ReviewListTotals::default();
        }
        app.needs_redraw = true;
        return;
    }
    let project = app.active_session().and_then(|s| s.project.clone());
    let workspace = app.workspace.clone();
    let branch = app.diff_overlay.as_ref().and_then(|o| o.branch.clone());
    let threads = match (project, branch, workspace) {
        (Some(project), Some(branch), Some(workspace)) => {
            match workspace.load_review_threads(&project, &branch) {
                Ok(threads) => threads,
                Err(error) => {
                    // Surface the failure via the banner rather than opening
                    // the list with silently-empty rollups.
                    if let Some(o) = app.diff_overlay.as_mut() {
                        o.review_load_error = Some(error);
                    }
                    app.needs_redraw = true;
                    return;
                }
            }
        }
        _ => Vec::new(),
    };
    let now = std::time::SystemTime::now();
    if let Some(o) = app.diff_overlay.as_mut() {
        o.review_rows = compute_review_rows(&o.reviews, &threads, now);
        o.review_totals = compute_review_totals(&o.reviews, &threads);
        o.reviews_selected = 0;
        o.reviews_open = true;
    }
    app.needs_redraw = true;
}

/// Route a key while the reviews list is open: `↑↓` move the highlight,
/// Enter navigates to the selected review's first comment, `l` / Esc
/// close the list.
pub(super) fn handle_reviews_list_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('l') => toggle_reviews_list(app),
        KeyCode::Up => {
            if let Some(o) = app.diff_overlay.as_mut() {
                o.reviews_selected = o.reviews_selected.saturating_sub(1);
                app.needs_redraw = true;
            }
        }
        KeyCode::Down => {
            if let Some(o) = app.diff_overlay.as_mut() {
                let last = o.review_rows.len().saturating_sub(1);
                o.reviews_selected = o.reviews_selected.saturating_add(1).min(last);
                app.needs_redraw = true;
            }
        }
        KeyCode::Enter => navigate_to_selected_review(app),
        _ => {}
    }
}

/// Close the list and scroll the diff to the selected review's first
/// comment: jump to its file when it's in the current scope, else switch
/// to its scope (the comment surfaces once that scan + hydrate land).
fn navigate_to_selected_review(app: &mut App) {
    let target = app.diff_overlay.as_ref().and_then(|o| {
        o.review_rows
            .get(o.reviews_selected)
            .map(|r| (r.first_commit.clone(), r.first_path.clone()))
    });
    if let Some(o) = app.diff_overlay.as_mut() {
        o.reviews_open = false;
        o.review_rows.clear();
        o.review_totals = ReviewListTotals::default();
    }
    app.needs_redraw = true;
    let Some((first_commit, first_path)) = target else { return };

    let scopes = app.diff_overlay.as_ref().map(|o| {
        let target_scope = match &first_commit {
            None => DiffScope::WholeDiff,
            Some(sha) => {
                o.commits.iter().position(|c| &c.sha == sha).map_or(o.scope, DiffScope::Commit)
            }
        };
        (target_scope, o.scope)
    });
    let Some((target_scope, current_scope)) = scopes else { return };
    if target_scope != current_scope {
        let outcome = app.diff_overlay.as_mut().map(|o| o.select_scope(target_scope));
        if let Some(outcome) = outcome {
            after_nav(app, outcome);
        }
        return;
    }
    if let Some(path) = first_path
        && let Some(o) = app.diff_overlay.as_mut()
        && let Some(file_idx) = o.files.iter().position(|f| f.path == path)
    {
        let file_start = o.doc_offsets().starts.get(file_idx).copied().unwrap_or(0);
        o.doc_scroll = o.message_rows.saturating_add(file_start);
    }
}

/// Whether this session has written a comment no review has sealed yet.
///
/// Read per thread and from the store, not from the cards. A thread shows
/// in every scope it belongs to and only the scope last entered was
/// rebuilt, so another scope's card can be a round behind - which is how a
/// comment resolved a moment ago still reads as work waiting to be filed.
/// `authored_this_session` is the one fact the store does not hold, so it
/// still comes off the card.
pub(crate) fn would_file(app: &App) -> bool {
    authored_threads(app).iter().any(ReviewThread::has_unfiled_user_turn)
}

/// This session's own threads, one per thread, each re-read from the
/// store so its state is the branch's rather than a card's.
fn authored_threads(app: &App) -> Vec<ReviewThread> {
    let Some(overlay) = app.diff_overlay.as_ref() else {
        return Vec::new();
    };
    // `Some` only when the store answered. An answer that does not list a
    // thread means it was deleted; no answer - no store, or a read that
    // failed - means the cards are all there is, and dropping them would
    // close over work that was never sealed.
    let answered: Option<Vec<ReviewThread>> = overlay
        .branch
        .as_ref()
        .zip(app.active_session().and_then(|s| s.project.clone()))
        .zip(app.workspace.as_ref())
        .and_then(|((branch, project), ws)| ws.load_review_threads(&project, branch).ok());
    let mut out: Vec<ReviewThread> = Vec::new();
    for card in overlay.comments.iter().filter(|c| c.authored_this_session) {
        if out.iter().any(|t| t.id == card.thread.id) {
            continue;
        }
        match answered.as_ref() {
            Some(stored) => match stored.iter().find(|t| t.id == card.thread.id) {
                Some(t) => out.push(t.clone()),
                // Absent from an answer that was given: deleted if it was
                // ever written, and never written otherwise - in which
                // case the card is still the only record of it.
                None if !card.persisted => out.push(card.thread.clone()),
                None => {}
            },
            None => out.push(card.thread.clone()),
        }
    }
    out
}

/// Whether this session wrote a comment that still wants attention on
/// submit: not resolved, and not drifted out from under its line. Read
/// per thread from the store for the same reason [`would_file`] is.
pub(super) fn session_work_pending(app: &App) -> bool {
    authored_threads(app)
        .iter()
        .any(|t| !matches!(t.status, ReviewStatus::Resolved | ReviewStatus::Outdated))
}

/// Close path for the overlay (banner ✕ click and `handle_key`'s Esc).
/// Opens the Finish-review modal only when at least one comment WOULD file
/// into a new review - authored this session AND carrying a user turn no
/// review has sealed. A reply on a thread already filed into an earlier
/// review counts: the conversation moved on and the new turn needs a round
/// of its own. An edit-only session (every authored turn already sealed)
/// and a look-only session both skip the modal and take the plain close
/// path: neither mints a review nor nudges the agent (edits are already
/// persisted; the agent reads them via the review MCP).
pub(super) fn close_with_submit(app: &mut App) {
    // Flush the active editor first - a reopened chip parks its saved
    // comment on `active_input.prior_comment`, so `overlay.comments` is
    // incomplete while the editor is open; the helper restores it.
    if let Some(o) = app.diff_overlay.as_mut() {
        let _ = super::comments::close_active_input_preserving_prior(o);
    }
    if would_file(app) {
        if let Some(o) = app.diff_overlay.as_mut() {
            o.finish_review = Some(FinishReviewState { editor: InputState::new() });
            app.needs_redraw = true;
        }
        return;
    }
    finalize_review_close(app, None, &[]);
}

/// Submit the Finish-review modal: seal this session's authored comments
/// into a fresh numbered review (with the optional overview), then nudge
/// the agent to address it via the review MCP and close.
pub(super) fn submit_finish_review(app: &mut App) {
    let overview =
        app.diff_overlay.as_ref().and_then(|o| o.finish_review.as_ref()).map(|f| f.editor.text());
    let overview = overview.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    let seal_ids: Vec<String> = app.diff_overlay.as_ref().map_or_else(Vec::new, |o| {
        let mut ids: Vec<String> = o
            .comments
            .iter()
            .filter(|c| c.authored_this_session)
            .map(|c| c.thread.id.clone())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    });
    finalize_review_close(app, overview.as_deref(), &seal_ids);
}

/// Shared tail for the Finish-review submit and the plain close: hold when
/// the agent isn't ready (surface stays open so notes survive), else seal
/// the listed still-unfiled threads into a numbered review (skipped when
/// `seal_ids` is empty - the edit-only / look-only path mints nothing) and
/// nudge the agent to address it through the review MCP. The overview is
/// stored on the review, never put in the chat (the agent reads it, and the
/// comments, via `review__get`). Sealing is best-effort: a session without
/// a branch / store still closes; only the local reviews-list record and
/// the nudge are lost.
fn finalize_review_close(app: &mut App, overview: Option<&str>, seal_ids: &[String]) {
    let pending = session_work_pending(app);
    if pending && (!app.has_active_agent() || app.session_id().is_none()) {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_close_held_no_agent",
            message = "diff review close held: agent not ready, comments preserved",
            outcome = "held",
            has_agent = app.has_active_agent(),
            has_session_id = app.session_id().is_some(),
        );
        crate::app::slash::push_system_message(
            app,
            "Review submit held: agent not ready. Wait for the session to connect, then Esc again to submit.",
        );
        app.needs_redraw = true;
        return;
    }

    // The reviewer's session is the notice target when a worker later
    // addresses this review; record it as the submit origin.
    let origin = app
        .active_session_key
        .clone()
        .unwrap_or_else(|| forge_workspace::SessionKey::from_session_id(String::new()));
    let project = app.active_session().and_then(|s| s.project.clone());
    let branch = app.diff_overlay.as_ref().and_then(|o| o.branch.clone());
    let workspace = app.workspace.clone();
    let review_number = if seal_ids.is_empty() {
        None
    } else if let (Some(project), Some(branch), Some(workspace)) = (&project, &branch, &workspace) {
        let (respond_tx, mut respond_rx) = tokio::sync::oneshot::channel();
        let review = workspace
            .dispatch(forge_workspace::Command::SubmitReview {
                project: project.clone(),
                branch: branch.clone(),
                summary: overview.map(str::to_owned),
                thread_ids: seal_ids.to_owned(),
                origin,
                respond: respond_tx,
            })
            .ok()
            .and_then(|()| respond_rx.try_recv().ok())
            .flatten();
        if review.is_none() {
            // The store write failed. Comments are already persisted unfiled
            // at save-time; only the local reviews-list record and the agent
            // nudge are lost. Degrade like the comment-save path: warn, close.
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_review_not_sealed",
                message = "diff review could not be sealed locally",
                outcome = "degraded",
            );
            crate::app::slash::push_system_message(
                app,
                "Review couldn't be saved locally (store error) - it won't show in the reviews list or reach the agent.",
            );
        }
        review.map(|r| r.number)
    } else {
        // There are comments to file but no (project, branch, workspace) to
        // file them under. They can't persist or reach the agent, so warn
        // like the store-fail path rather than dropping them silently (the
        // pre-nudge bundle dispatched regardless of branch). Name the step
        // that came up empty: the three collapse to very different fixes,
        // and only the middle one is about HEAD.
        let missing = if project.is_none() {
            "this session is not under a forge project"
        } else if branch.is_none() {
            "the checkout has no branch name - a detached HEAD, or git could not read it (the log carries which)"
        } else {
            "forge is shutting down"
        };
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_review_scope_unresolved",
            message = "diff review not sealed: incomplete scope",
            outcome = "dropped",
            has_project = project.is_some(),
            has_branch = branch.is_some(),
            has_workspace = workspace.is_some(),
        );
        crate::app::slash::push_system_message(
            app,
            format!(
                "Can't file a review here: {missing} - comments won't persist or reach the agent."
            ),
        );
        None
    };

    // A freshly-sealed review with something to act on nudges the agent to
    // read + address it via the review MCP - one line, not the comments.
    if pending && let Some(number) = review_number {
        crate::app::input_submit::dispatch_review_nudge(
            app,
            format!("Review #{number} ready - address it via the review MCP (`review__list`)."),
        );
    }
    super::lifecycle::close(app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diff_overlay::comments::save_active_input;
    use crate::app::diff_overlay::mouse::reopen_comment_for_turn;
    use crate::app::diff_overlay::state::DiffOverlayState;
    use crate::app::diff_overlay::test_support::*;
    use crate::app::diff_overlay::threads::{apply_thread_action, hydrate_threads};
    use crate::app::diff_overlay::types::{
        ActiveCommentInput, CachedScan, CommentRef, HunkComment, LineKey, ThreadAction,
    };
    use crate::app::view::{ActiveView, set_active_view};
    use forge_primitives::review::{ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide};
    use forge_workspace::env::git_diff::resolver;
    use std::path::PathBuf;

    #[test]
    fn close_with_submit_opens_finish_review_when_authored() {
        // A session that authored a comment opens the Finish-review modal
        // on close instead of closing - the pass seals into a review on
        // exit. Agent-agnostic: the modal opens whether or not the agent
        // is ready (the send happens on submit).
        let mut app = App::test_default();
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            comment_text: "needs unwrap fix".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        let overlay = app.diff_overlay.as_ref().expect("overlay stays open");
        assert!(overlay.finish_review.is_some(), "the Finish-review modal opened");
        assert_eq!(overlay.comments.len(), 1, "the authored comment is preserved");
        assert_eq!(app.active_view, ActiveView::Diff, "view stays on Diff");
    }

    #[test]
    fn close_with_submit_closes_directly_when_look_only() {
        // A look-only session (only hydrated comments, nothing authored
        // this session) closes straight through - no modal, no re-send.
        let mut app = App::test_default();
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            comment_text: "hydrated from a prior review".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "look-only close drops the overlay");
        assert_eq!(app.active_view, ActiveView::Chat, "view returns to chat");
    }

    #[test]
    fn close_with_submit_edit_only_no_ops() {
        // A session that only edits an already-filed comment must NOT mint a
        // review (the modal never opens) and must NOT dispatch anything: the
        // edit is already persisted, and the agent reads it via the review
        // MCP - there is nothing to nudge.
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        let mut thread = filed_thread("rev");
        thread.id = "filed".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "tweaked note".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "edit-only close skips the modal and closes");
        assert!(rx.try_recv().is_err(), "an edit-only close dispatches nothing to the agent");
        assert!(
            ws.load_reviews("forge", "feat").expect("load").is_empty(),
            "no review was minted for an edit-only session",
        );
    }

    /// A review conversation spans several rounds: the reviewer comments,
    /// the agent answers, the reviewer answers back. That second reply is a
    /// new unfiled turn, so Esc must offer the modal and Submit must seal a
    /// second review the thread also belongs to.
    #[test]
    fn a_reply_on_a_filed_thread_seals_into_a_second_review() {
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let origin = forge_workspace::SessionKey::from_session_id("review-session");

        let mut thread = user_thread("does this handle the empty case?");
        thread.id = "t1".to_owned();
        ws.save_review_threads("forge", "feat", &[thread]);
        let r1 = ws
            .submit_review("forge", "feat", None, &["t1".to_owned()], origin.clone())
            .expect("first review sealed");

        // The agent answers, which flips the thread to Addressed.
        let status = ws
            .review_reply(&origin, "forge", "feat", "t1", "implementer", "fixed in b3f1", "")
            .expect("agent reply");
        assert_eq!(status, ReviewStatus::Addressed);

        // The reviewer answers back on the already-filed thread.
        let mut replied =
            ws.load_review_threads("forge", "feat").expect("load").pop().expect("thread");
        replied.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "the empty case is still unguarded".to_owned(),
            at: String::new(),
            review_id: None,
        });
        assert!(ws.upsert_review_thread("forge", "feat", replied.clone()), "reply persisted");

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.reviews = ws.load_reviews("forge", "feat").expect("load reviews");
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "does this handle the empty case?".into(),
            commit: None,
            thread: replied,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "a reply on a filed thread is unfiled work, so the modal opens",
        );
        submit_finish_review(&mut app);

        let reviews = ws.load_reviews("forge", "feat").expect("load reviews");
        assert_eq!(reviews.len(), 2, "the reply sealed a second review");
        let r2 = &reviews[1];
        assert_eq!(r2.number, 2);

        let stored = ws
            .load_review_threads("forge", "feat")
            .expect("load")
            .into_iter()
            .find(|t| t.id == "t1")
            .expect("thread");
        assert!(stored.is_in_review(&r1.id), "the thread stays in the first review");
        assert!(stored.is_in_review(&r2.id), "and now also belongs to the second");
        assert_eq!(
            stored.comments.iter().map(|c| c.review_id.as_deref()).collect::<Vec<_>>(),
            vec![Some(r1.id.as_str()), None, Some(r2.id.as_str())],
            "each turn carries the review that sealed it; the agent reply carries none",
        );
        assert_eq!(
            stored.status,
            ReviewStatus::Open,
            "the agent owes another answer, so sealing reopens the thread",
        );
        assert!(rx.try_recv().is_ok(), "the second review nudges the agent");
    }

    #[test]
    fn submit_finish_review_files_a_resolved_comment_without_dispatch() {
        // An authored NEW comment resolved before close still trips
        // would_file (it's unfiled), so the modal opens and Submit mints a
        // review filing the Resolved comment - but a resolved comment isn't
        // actionable, so no nudge is dispatched to the agent.
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let mut seeded = stock_thread();
        seeded.id = "r".to_owned();
        seeded.status = ReviewStatus::Resolved;
        ws.save_review_threads("forge", "feat", &[seeded.clone()]);

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "resolved before close".into(),
            commit: None,
            thread: seeded,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "an unfiled authored comment opens the modal even when resolved",
        );
        submit_finish_review(&mut app);

        assert!(app.diff_overlay.is_none(), "overlay closed on submit");
        let reviews = ws.load_reviews("forge", "feat").expect("load reviews");
        assert_eq!(reviews.len(), 1, "a review was minted");
        let filed = ws
            .load_review_threads("forge", "feat")
            .expect("load")
            .into_iter()
            .find(|t| t.id == "r")
            .expect("thread")
            .origin_review()
            .map(str::to_owned);
        assert_eq!(
            filed,
            Some(reviews[0].id.clone()),
            "the resolved comment filed into the review"
        );
        assert!(rx.try_recv().is_err(), "a resolved comment is not dispatched to the agent");
    }

    #[test]
    fn submit_finish_review_degrades_when_the_seal_fails() {
        // When the seal write fails (here a corrupt threads row rolls back
        // the submit txn, so submit_review returns None) there is no review
        // number to nudge with, so submit degrades gracefully: it closes
        // (never holds - a store-down session would dead-end), dispatches no
        // nudge, and pushes a system message so the failure isn't silent.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("ws");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_row_for_test(&db, "forge", "feat")
            .expect("corrupt row");
        workspace.install_db_for_test(db);
        let mut rx = app.install_testing_stub();
        app.set_session_id(Some(crate::agent::model::SessionId::new("review-session")));
        if let Some(key) = app.active_session_key.clone()
            && let Some(session) = app.sessions.get_mut(&key)
        {
            session.project = Some("forge".to_owned());
            session.cwd_raw = "/tmp/repo".into();
        }

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.finish_review = Some(FinishReviewState { editor: InputState::new() });
        let mut thread = stock_thread();
        thread.id = "fresh".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "note".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        submit_finish_review(&mut app);

        assert!(
            app.diff_overlay.is_none(),
            "the overlay closes (no dead-end hold) on a seal failure"
        );
        assert!(
            rx.try_recv().is_err(),
            "a failed seal has no review number, so nothing is nudged to the agent",
        );
        assert!(
            app.messages().iter().any(|m| matches!(m.role, crate::app::MessageRole::System(None))),
            "a system message warns that the review wasn't saved locally",
        );
    }

    #[test]
    fn submit_finish_review_on_detached_head_warns_not_silently_drops() {
        // A detached HEAD leaves `overlay.branch == None`, so the review has
        // no (project, branch) to file under. With pending comments + a ready
        // agent it must NOT silently drop: it closes but pushes a system
        // message so the loss is visible (mirrors the store-fail branch).
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        // No branch set -> detached HEAD.
        overlay.finish_review = Some(FinishReviewState { editor: InputState::new() });
        let mut thread = stock_thread();
        thread.id = "fresh".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "note".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        submit_finish_review(&mut app);

        assert!(app.diff_overlay.is_none(), "the overlay closes (no dead-end hold)");
        assert!(rx.try_recv().is_err(), "no branch to file under, so nothing is dispatched");
        let notice = system_notice_text(&app).expect("a system message warns about the loss");
        assert!(
            notice.contains("branch name"),
            "the notice names the step that came up empty: {notice}",
        );
    }

    /// The three ways the submit scope comes up empty need three
    /// different fixes, and only the middle one is about HEAD. All three
    /// used to say "no branch - detached HEAD?", the same guess the read
    /// side dropped.
    #[test]
    fn an_unresolved_submit_scope_names_the_step_that_failed_not_head() {
        let (mut app, mut rx, _dir) = review_app_with_agent();
        // Project unset: the session is not under a forge project at all,
        // which has nothing to do with the checkout's HEAD.
        if let Some(key) = app.active_session_key.clone()
            && let Some(session) = app.sessions.get_mut(&key)
        {
            session.project = None;
        }
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.finish_review = Some(FinishReviewState { editor: InputState::new() });
        let mut thread = stock_thread();
        thread.id = "fresh".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "note".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        submit_finish_review(&mut app);

        assert!(rx.try_recv().is_err(), "nothing to file under, so nothing is dispatched");
        let notice = system_notice_text(&app).expect("a system message warns about the loss");
        assert!(notice.contains("forge project"), "the notice names the project step: {notice}");
        assert!(!notice.contains("detached"), "a missing project is not a detached HEAD: {notice}");
    }

    #[test]
    fn submit_finish_review_flushes_reopened_chip_before_seal() {
        // A chip-reopen with an open editor must restore the prior on close
        // so it counts as an actionable comment on submit; without the flush
        // `overlay.comments` is empty while the editor is open, the review
        // seals nothing actionable, and no nudge fires. The nudge dispatched
        // here proves the flush ran.
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let mut state = sample_state();
        state.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "important review note".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        let mut editor = InputState::new();
        editor.insert_str("important review note");
        // Editor open as a chip reopen - prior_comment Some, no
        // unsubmitted comments in overlay.comments yet.
        state.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior.clone()),
            edit_turn: Some(0),
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        // Close flushes the editor (restoring the prior) and opens the
        // modal because the prior is authored this session.
        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "the Finish-review modal opened",
        );
        submit_finish_review(&mut app);
        assert!(app.diff_overlay.is_none(), "overlay closed on submit");
        // The flush restored an actionable comment, so a nudge fired.
        match rx.try_recv().expect("a nudge was dispatched") {
            forge_primitives::AgentCommand::PromptWithImages { text, .. } => {
                assert!(
                    text.contains("Review #1") && text.contains("review__list"),
                    "the nudge points at the sealed review, got: {text}",
                );
            }
            other => panic!("expected PromptWithImages, got {other:?}"),
        }
    }

    #[test]
    fn submit_finish_review_holds_when_agent_not_ready() {
        // Submitting a review with sendable comments but no ready agent
        // must NOT close - it holds (modal stays, comments preserved,
        // nothing dispatched) so the notes survive until the session
        // connects. Mirrors the pre-modal no-agent guard at the new
        // submit layer.
        let mut app = App::test_default();
        // No install_testing_stub → has_active_agent = false.
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            comment_text: "to be preserved".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "the modal opened",
        );
        submit_finish_review(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay held open");
        assert!(after.finish_review.is_some(), "modal stays open on the no-agent hold");
        assert_eq!(after.comments.len(), 1, "the comment is preserved");
        assert_eq!(after.comments[0].comment_text, "to be preserved");
        assert_eq!(app.active_view, ActiveView::Diff, "view stays on Diff");
    }

    #[test]
    fn close_with_submit_no_comments_closes_cleanly_even_without_agent() {
        // Empty comments path skips the dispatch entirely, so the
        // no-agent state shouldn't block closing - the user just
        // wants to dismiss the overlay.
        let mut app = App::test_default();
        app.diff_overlay = Some(sample_state());
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "empty-comments close still drops state");
        assert_eq!(app.active_view, ActiveView::Chat, "view returns to chat");
    }

    #[test]
    fn submit_finish_review_seals_files_and_nudges() {
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(
            &mut overlay,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "bound check?",
        );
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        close_with_submit(&mut app);
        if let Some(o) = app.diff_overlay.as_mut() {
            o.finish_review.as_mut().expect("modal open").editor.insert_str("Solid overall.");
        }
        submit_finish_review(&mut app);
        assert!(app.diff_overlay.is_none(), "overlay closed on submit");

        let reviews = ws.load_reviews("forge", "feat").expect("load reviews");
        assert_eq!(reviews.len(), 1, "a review was sealed");
        assert_eq!(reviews[0].number, 1);
        assert_eq!(reviews[0].summary.as_deref(), Some("Solid overall."), "overview stored");
        let threads = ws.load_review_threads("forge", "feat").expect("load threads");
        assert_eq!(
            threads[0].origin_review(),
            Some(reviews[0].id.as_str()),
            "the session comment filed into the review",
        );

        let dispatched = rx.try_recv().expect("nudge dispatched");
        match dispatched {
            forge_primitives::AgentCommand::PromptWithImages { text, .. } => {
                assert!(text.contains("Review #1"), "the nudge names the sealed review");
                assert!(text.contains("review__list"), "the nudge points at the review MCP");
                // The overview and comment text stay OUT of the chat - the
                // agent reads them via review__get.
                assert!(!text.contains("Solid overall."), "overview stays out of the chat");
                assert!(!text.contains("bound check?"), "comment text stays out of the chat");
            }
            other => panic!("expected PromptWithImages, got {other:?}"),
        }
    }

    #[test]
    fn submit_finish_review_files_only_this_sessions_comments() {
        let (mut app, _rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 10,
                content_hash: 0,
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: "note".to_owned(),
                at: "t0".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: None,
        };
        // Both threads exist in redb; the overlay carries one authored this
        // session and one hydrated from a prior pass.
        ws.save_review_threads("forge", "feat", &[seed("authored"), seed("hydrated")]);
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let comment = |line_idx: usize, id: &str, authored: bool| HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx },
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "note".into(),
            commit: None,
            thread: seed(id),
            authored_this_session: authored,
            anchor_note: None,
            persisted: true,
        };
        overlay.comments.push(comment(0, "authored", true));
        overlay.comments.push(comment(1, "hydrated", false));
        app.diff_overlay = Some(overlay);

        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "an authored comment opens the modal",
        );
        submit_finish_review(&mut app);

        let threads = ws.load_review_threads("forge", "feat").expect("load");
        let is_filed = |id: &str| {
            threads.iter().find(|t| t.id == id).expect("thread present").origin_review().is_some()
        };
        assert!(is_filed("authored"), "the session-authored comment filed into the review");
        assert!(!is_filed("hydrated"), "the hydrated comment was NOT swept into the review");
    }

    #[test]
    fn compute_review_rows_tallies_newest_first() {
        let reviews = vec![
            forge_primitives::ReviewSet {
                id: "r1".to_owned(),
                number: 1,
                summary: Some("first pass".to_owned()),
                created_at: "2026-07-23T08:00:00Z".to_owned(),
            },
            forge_primitives::ReviewSet {
                id: "r2".to_owned(),
                number: 2,
                summary: None,
                created_at: "2026-07-23T10:00:00Z".to_owned(),
            },
        ];
        let mk = |id: &str, review: &str, status: ReviewStatus| {
            let mut t = filed_thread(review);
            t.id = id.to_owned();
            t.status = status;
            t.anchor.path = "src/a.rs".to_owned();
            t
        };
        let threads = vec![
            mk("a", "r1", ReviewStatus::Resolved),
            mk("b", "r1", ReviewStatus::Open),
            mk("d", "r1", ReviewStatus::Addressed),
            mk("c", "r2", ReviewStatus::Outdated),
        ];
        let now = parse_rfc3339("2026-07-23T12:00:00Z").expect("now parses");
        let rows = compute_review_rows(&reviews, &threads, now);

        assert_eq!(rows.len(), 2);
        // Newest review first.
        assert_eq!(rows[0].number, 2, "review 2 leads");
        assert_eq!(rows[0].total, 1);
        assert_eq!(rows[0].outdated, 1);
        assert_eq!(rows[0].age, "2h", "created two hours before now");
        assert_eq!(rows[1].number, 1);
        assert_eq!(rows[1].total, 3, "all three r1 threads tally");
        assert_eq!(rows[1].open, 1);
        assert_eq!(rows[1].addressed, 1, "the addressed thread tallies into its own bucket");
        assert_eq!(rows[1].resolved, 1);
        assert_eq!(rows[1].summary.as_deref(), Some("first pass"));
        assert_eq!(rows[1].first_path.as_deref(), Some("src/a.rs"));

        let totals = compute_review_totals(&reviews, &threads);
        assert_eq!(totals.comments, 4, "four distinct filed comments");
        assert_eq!((totals.open, totals.addressed), (1, 1));
    }

    /// A thread the reviewer replied on across rounds is listed under every
    /// review it has a turn in, and counted once in the footer.
    #[test]
    fn a_multi_round_thread_is_listed_under_each_of_its_reviews() {
        let reviews = vec![
            forge_primitives::ReviewSet {
                id: "r1".to_owned(),
                number: 1,
                summary: None,
                created_at: "2026-07-23T08:00:00Z".to_owned(),
            },
            forge_primitives::ReviewSet {
                id: "r2".to_owned(),
                number: 2,
                summary: None,
                created_at: "2026-07-23T10:00:00Z".to_owned(),
            },
        ];
        let mut spanning = filed_thread("r1");
        spanning.id = "spanning".to_owned();
        spanning.comments.push(agent_turn("addressed"));
        spanning.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "still not right".to_owned(),
            at: String::new(),
            review_id: Some("r2".to_owned()),
        });
        let threads = vec![spanning];
        let now = parse_rfc3339("2026-07-23T12:00:00Z").expect("now parses");

        let rows = compute_review_rows(&reviews, &threads, now);
        assert_eq!(rows[0].total, 1, "r2 lists it");
        assert_eq!(rows[1].total, 1, "and so does r1");
        assert_eq!(
            compute_review_totals(&reviews, &threads).comments,
            1,
            "one comment, not one per review it appears in",
        );
    }

    #[test]
    fn toggle_reviews_list_opens_with_rows_then_closes() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let mut filed = filed_thread("rev");
        filed.id = "a".to_owned();
        ws.save_review_threads("forge", "feat", &[filed]);

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.reviews = vec![forge_primitives::ReviewSet {
            id: "rev".to_owned(),
            number: 1,
            summary: None,
            created_at: String::new(),
        }];
        app.diff_overlay = Some(overlay);

        toggle_reviews_list(&mut app);
        let o = app.diff_overlay.as_ref().expect("overlay");
        assert!(o.reviews_open, "the list opened");
        assert_eq!(o.review_rows.len(), 1);
        assert_eq!(o.review_rows[0].total, 1, "the filed thread tallies into the review");

        toggle_reviews_list(&mut app);
        assert!(!app.diff_overlay.as_ref().expect("overlay").reviews_open, "toggle closes it");
    }

    #[test]
    fn toggle_reviews_list_surfaces_a_load_error() {
        // The rollup needs every thread; a corrupt threads row must surface
        // the banner, not open a list with silently-empty rollups.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("ws");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_row_for_test(&db, "forge", "feat")
            .expect("corrupt row");
        workspace.install_db_for_test(db);
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        toggle_reviews_list(&mut app);

        let o = app.diff_overlay.as_ref().expect("overlay");
        assert!(!o.reviews_open, "the list does not open on a thread-load failure");
        assert!(o.review_load_error.is_some(), "the failure surfaces via the banner");
    }

    #[test]
    fn a_comment_written_this_session_stays_submittable_across_a_scope_round_trip() {
        // The rebuild reinstates cards from the store, and the store has
        // no notion of "written in this overlay session". Losing that flag
        // takes the comment out of the Finish-review modal and out of the
        // review it should seal into - silently, since the card still
        // renders.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files.clone());
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.commit_cache = vec![Some(CachedScan { files: files.clone(), scanner_ok: true })];
        overlay.whole_diff_cache = Some(CachedScan { files, scanner_ok: true });
        overlay.scope = DiffScope::WholeDiff;
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "worth a second look");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        assert!(session_work_pending(&app), "the comment is submittable the moment it is written");

        for scope in [DiffScope::Commit(0), DiffScope::WholeDiff] {
            let outcome = app.diff_overlay.as_mut().expect("overlay").select_scope(scope);
            after_nav(&mut app, outcome);
        }

        assert!(
            session_work_pending(&app),
            "and still is after looking at a commit and coming back",
        );
    }

    #[test]
    fn a_thread_loaded_from_history_is_not_session_work() {
        // The other half: a rebuild must not promote a thread the
        // reviewer never touched, or reopening a branch would re-nudge
        // the agent about comments from an earlier pass.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let mut thread = stock_thread();
        thread.id = "from-history".to_owned();
        thread.anchor = ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            line: 5,
            content_hash: resolver::anchor_hash("let a = 1;"),
            context: Vec::new(),
            base_ref: "main".to_owned(),
        };
        ws.save_review_threads("forge", "feat", &[thread]);
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        assert!(!session_work_pending(&app), "nothing here is this session's work");
    }

    #[test]
    fn session_work_survives_a_round_trip_through_a_scope_that_has_threads() {
        // The R4 property, but stepping through a scope that HAS a thread
        // so hydrate cannot take its nothing-in-scope early return.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let mut homed = cross_numbered_thread();
        homed.id = "in-target-scope".to_owned();
        ws.save_review_threads("forge", "feat", &[homed]);
        let mut overlay = cross_numbered_overlay();
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        with_editor(&mut overlay, key, "worth a second look");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        assert!(session_work_pending(&app), "submittable when written");

        for scope in [DiffScope::Commit(0), DiffScope::WholeDiff] {
            let outcome = app.diff_overlay.as_mut().expect("o").select_scope(scope);
            after_nav(&mut app, outcome);
        }

        assert!(
            session_work_pending(&app),
            "still session work after a round trip through a scope that has its own threads",
        );
    }

    #[test]
    fn a_resolved_comment_does_not_keep_the_overlay_open_from_a_stale_card() {
        // The reported journey. Comment on a commit, look at All changes
        // (which draws the same thread as a second card), go back to the
        // commit and resolve. The whole-diff card is a scope behind and
        // still reads Open, so the close path sees work to file for a
        // review whose only comment is resolved.
        let (mut app, _dir) = review_app();
        let mut overlay = cross_numbered_overlay();
        overlay.scope = DiffScope::Commit(0);
        overlay.files = overlay.commit_cache[0].as_ref().expect("cached").files.clone();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        with_editor(&mut overlay, key, "why this cast?");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        for scope in [DiffScope::WholeDiff, DiffScope::Commit(0)] {
            let outcome = app.diff_overlay.as_mut().expect("o").select_scope(scope);
            after_nav(&mut app, outcome);
        }
        let overlay = app.diff_overlay.as_ref().expect("o");
        assert!(
            overlay.comments.len() >= 2,
            "the thread has a card in each scope visited; got {}",
            overlay.comments.len(),
        );
        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);

        assert!(
            !session_work_pending(&app),
            "the thread is resolved in the store; a card left in another scope does not outvote it",
        );
    }

    #[test]
    fn deleting_a_comment_from_one_view_does_not_leave_it_owed_by_the_other() {
        // Clearing a comment's only turn removes the thread from the
        // store, but reopening removes one card and a thread drawing in
        // two views leaves the other standing. Resurrecting from that
        // card mints a review with no members.
        let (mut app, _dir) = review_app();
        let mut overlay = cross_numbered_overlay();
        overlay.scope = DiffScope::Commit(0);
        overlay.files = overlay.commit_cache[0].as_ref().expect("cached").files.clone();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        with_editor(&mut overlay, key, "why this cast?");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        let outcome = app.diff_overlay.as_mut().expect("o").select_scope(DiffScope::WholeDiff);
        after_nav(&mut app, outcome);

        // Clear its only turn from the whole diff: the thread is deleted.
        let overlay = app.diff_overlay.as_mut().expect("o");
        reopen_comment_for_turn(overlay, CommentRef { line: key, slot: 0 }, Some(0));
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor = InputState::new();
        }
        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        assert!(
            ws.load_review_threads("forge", "feat").expect("load").is_empty(),
            "the thread is gone from the store",
        );
        assert!(
            !would_file(&app),
            "so nothing is owed - a card left in another view must not bring it back",
        );
    }

    #[test]
    fn a_store_that_cannot_answer_leaves_the_cards_standing() {
        // The opposite direction. A read failure is not an answer, so it
        // must not read as "every thread was deleted" and close over work
        // that was never sealed. The card is `persisted`, so a rule that
        // only rescued unwritten cards would drop this one.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("ws");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_row_for_test(&db, "forge", "feat")
            .expect("corrupt row");
        ws.install_db_for_test(db);
        if let Some(key) = app.active_session_key.clone()
            && let Some(session) = app.sessions.get_mut(&key)
        {
            session.project = Some("forge".to_owned());
        }
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let mut thread = stock_thread();
        thread.id = "written-earlier".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 5,
            comment_text: "worth a second look".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        assert!(
            would_file(&app),
            "the store could not answer, so the cards stand rather than reading as deleted",
        );
    }

    #[test]
    fn the_footer_offers_a_review_only_when_esc_would_open_one() {
        // The hint and the key have to agree. A thread deleted from
        // another view leaves its card standing here, so reading the
        // cards offered a review that Esc then declined to open.
        let (mut app, _dir) = review_app();
        let mut overlay = cross_numbered_overlay();
        overlay.scope = DiffScope::Commit(0);
        overlay.files = overlay.commit_cache[0].as_ref().expect("cached").files.clone();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        with_editor(&mut overlay, key, "why this cast?");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        assert!(would_file(&app), "written and unsealed");

        let outcome = app.diff_overlay.as_mut().expect("o").select_scope(DiffScope::WholeDiff);
        after_nav(&mut app, outcome);
        let overlay = app.diff_overlay.as_mut().expect("o");
        reopen_comment_for_turn(overlay, CommentRef { line: key, slot: 0 }, Some(0));
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor = InputState::new();
        }
        save_active_input(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("o");
        assert!(
            overlay.comments.iter().any(|c| c.authored_this_session),
            "a card is still standing, which is what made the footer disagree",
        );
        assert!(!would_file(&app), "but the thread is gone, so Esc will just close");
    }

    #[test]
    fn session_work_pending_covers_who_wrote_it_and_what_state_it_is_in() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, status: ReviewStatus| {
            let mut t = cross_numbered_thread();
            t.id = id.to_owned();
            t.commit = None;
            t.status = status;
            t
        };
        let case = |app: &mut App, authored: bool, status: ReviewStatus| {
            ws.save_review_threads("forge", "feat", &[seed("t", status)]);
            let mut overlay = cross_numbered_overlay();
            let mut thread = seed("t", status);
            thread.status = ReviewStatus::Open; // a card can lag the store
            overlay.comments.push(HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 },
                path: "src/x.rs".into(),
                line: 5,
                comment_text: "c".into(),
                commit: None,
                thread,
                authored_this_session: authored,
                anchor_note: None,
                persisted: true,
            });
            app.diff_overlay = Some(overlay);
            session_work_pending(app)
        };

        assert!(case(&mut app, true, ReviewStatus::Open), "written here and open: wants attention");
        assert!(
            !case(&mut app, false, ReviewStatus::Open),
            "loaded from an earlier pass: never re-nudged however open it is",
        );
        assert!(
            !case(&mut app, true, ReviewStatus::Resolved),
            "resolved in the store outranks a card that has not caught up",
        );
        assert!(!case(&mut app, true, ReviewStatus::Outdated), "and so does drift");
    }
}
