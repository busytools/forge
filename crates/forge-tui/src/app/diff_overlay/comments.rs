//! The comment editor's lifecycle: closing it with the saved comment
//! preserved, and saving the text into a durable [`ReviewThread`] on
//! the workspace command bus.

use super::state::DiffOverlayState;
use super::threads::refresh_replies_waiting;
use super::types::HunkComment;
use crate::app::App;
use forge_primitives::review::{
    ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus,
};
use forge_workspace::env::git_diff::hunks::DiffLineKind;
use forge_workspace::env::git_diff::resolver::{self, CONTEXT_RADIUS};

/// Close the active comment editor (if any), restoring its
/// `prior_comment` when the editor was opened by re-clicking a saved
/// chip. Called everywhere `active_input` is dropped or replaced:
/// Esc-cancel, clicking a different diff line, clicking a different
/// chip, switching files via the rail, narrow-tier arrow clicks.
/// Without this centralization, every dismissal path that bypasses
/// Esc would silently destroy the saved comment - the exact bug
/// `prior_comment` was added to prevent.
///
/// Logs DEBUG with the abandoned char count when text is dropped
/// (fresh draft, or modifications layered on a reopened chip), so
/// a "where did my edit go?" triage can correlate from logs.
/// Returns the abandoned count as a Unicode scalar count for
/// callers that want it - most don't, but the central log fires
/// regardless.
pub(super) fn close_active_input_preserving_prior(overlay: &mut DiffOverlayState) -> usize {
    let Some(input) = overlay.active_input.take() else { return 0 };
    let current_text = input.editor.text();
    // Two abandonment shapes:
    // - Fresh draft (`prior_comment = None`): every char is lost
    //   on dismissal.
    // - Reopened chip with user edits: the editor seeded from the
    //   prior, then diverged. We restore the prior verbatim on
    //   dismissal (matches GitHub edit-modal semantics: Esc =
    //   discard changes), so the divergence is the user's typed-
    //   over text that gets dropped.
    let abandoned = match input.prior_comment.as_ref() {
        Some(prior) if current_text != prior.comment_text => current_text.chars().count(),
        Some(_) => 0,
        None => current_text.chars().count(),
    };
    if abandoned > 0 {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_editor_dropped_in_progress",
            message = "comment editor closed with unsaved text",
            outcome = "dropped",
            abandoned_chars = abandoned,
            had_prior = input.prior_comment.is_some(),
        );
    }
    if let Some(prior) = input.prior_comment {
        overlay.comments.push(prior);
        overlay.recompute_comment_counts();
    }
    abandoned
}

/// Discard the active comment editor without saving. If the editor
/// was opened by re-clicking a saved 💬 chip, restore the original
/// comment so the chip reappears - the user clicked to view/edit,
/// not to destroy. Fresh line-click editors have no prior to
/// restore; their in-progress text is discarded (with the helper's
/// central DEBUG log noting the abandoned char count).
pub(super) fn cancel_active_input(app: &mut App) {
    if let Some(overlay) = app.diff_overlay.as_mut() {
        let _ = close_active_input_preserving_prior(overlay);
        app.needs_redraw = true;
    }
}

/// Persist the active editor's text into [`DiffOverlayState::comments`]
/// and close the editor. The snapshot includes the anchor line's
/// hunk context so the captured context stays stable even if the
/// user scrolls / switches files later.
///
/// Empty-text save semantics:
/// - Fresh line-click editor (no `prior_comment`): treated as
///   cancel - saving a blank comment would render an empty chip.
/// - Turn-edit editor (`prior_comment` + `edit_turn = Some(idx)` on a
///   `User` turn): clearing removes just THAT turn and re-saves the
///   surviving chain, so an earlier note and any worker replies stay.
///   The whole thread is deleted only when no `User` turn would remain
///   (so an orphaned agent reply never lingers).
/// - Reply editor (`prior_comment` + `edit_turn = None`), or a clear
///   aimed at a non-editable turn: restore the prior untouched (no
///   delete, no new turn).
pub(super) fn save_active_input(app: &mut App) {
    persist_active_input(app);
    // A reviewer turn on an answered thread hands the ball back to the
    // worker, and a cleared turn can hand it the other way.
    refresh_replies_waiting(app);
}

fn persist_active_input(app: &mut App) {
    // Project name is read before the overlay borrow so the persist call
    // below can reach `app.workspace` without a borrow conflict.
    let project = app.active_session().and_then(|s| s.project.clone());
    let workspace = app.workspace.clone();
    let Some(overlay) = app.diff_overlay.as_mut() else { return };
    let branch = overlay.branch.clone();
    let Some(input) = overlay.active_input.take() else { return };
    let text = input.editor.text();
    if text.trim().is_empty() {
        let edit_turn = input.edit_turn;
        if let Some(mut prior) = input.prior_comment {
            let clears_user_turn = edit_turn
                .and_then(|idx| prior.thread.comments.get(idx))
                .is_some_and(|c| matches!(c.author, ReviewAuthor::User));
            if let (true, Some(idx)) = (clears_user_turn, edit_turn) {
                prior.thread.comments.remove(idx);
                let user_turn_remains =
                    prior.thread.comments.iter().any(|c| matches!(c.author, ReviewAuthor::User));
                if user_turn_remains {
                    // Trim just this turn; re-save the surviving chain.
                    let persisted = if let (Some(project), Some(branch), Some(workspace)) =
                        (project.as_deref(), branch.as_deref(), workspace.as_ref())
                    {
                        let (respond_tx, mut respond_rx) = tokio::sync::oneshot::channel();
                        workspace
                            .dispatch(forge_workspace::Command::UpsertReviewThread {
                                project: project.to_owned(),
                                branch: branch.to_owned(),
                                thread: prior.thread.clone(),
                                respond: respond_tx,
                            })
                            .ok()
                            .and_then(|()| respond_rx.try_recv().ok())
                            .unwrap_or_else(|| {
                                tracing::warn!(
                                    target: crate::logging::targets::APP_SESSION,
                                    event_name = "diff_overlay_review_thread_not_persisted",
                                    message = "trimmed review thread persistence unconfirmed on the bus; kept in-memory only",
                                    outcome = "at_risk",
                                );
                                false
                            })
                    } else {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "diff_overlay_review_thread_not_persisted",
                            message = "trimmed review thread could not be persisted (no branch / project / store); kept in-memory only",
                            outcome = "at_risk",
                            has_branch = branch.is_some(),
                            has_project = project.is_some(),
                        );
                        false
                    };
                    prior.comment_text = prior
                        .thread
                        .comments
                        .iter()
                        .find(|c| matches!(c.author, ReviewAuthor::User))
                        .map_or_else(String::new, |c| c.text.clone());
                    prior.persisted = persisted;
                    overlay.comments.push(prior);
                    overlay.recompute_comment_counts();
                } else {
                    // No user turn left: delete the whole thread (durable too,
                    // else hydrate resurrects it next open).
                    if let (Some(project), Some(branch), Some(workspace)) =
                        (project.as_deref(), branch.as_deref(), workspace.as_ref())
                    {
                        let _ = workspace.dispatch(forge_workspace::Command::RemoveReviewThread {
                            project: project.to_owned(),
                            branch: branch.to_owned(),
                            thread_id: prior.thread.id.clone(),
                        });
                    } else {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "diff_overlay_review_thread_not_removed",
                            message = "review thread delete skipped (no branch / project / store); may resurrect on next open",
                            outcome = "skipped",
                            has_branch = branch.is_some(),
                            has_project = project.is_some(),
                        );
                    }
                }
            } else {
                // Empty reply, or a clear aimed at a non-editable turn:
                // restore the prior untouched.
                overlay.comments.push(prior);
                overlay.recompute_comment_counts();
            }
        }
        app.needs_redraw = true;
        return;
    }
    // Resolve the line key into a snapshot. Under correct contract
    // (body immutable within one open) these get-branches are dead;
    // WARN-log them so a future regression that violates the
    // contract is observable, with the lengths in the log so a
    // post-mortem can quantify lost user text.
    let key = input.key;
    let Some(file) = overlay.files.get(key.file_idx) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_save_oob_file_idx",
            message = "save_active_input hit oob file_idx - body mutated mid-open?",
            outcome = "skipped",
            file_idx = key.file_idx,
            file_count = overlay.files.len(),
            lost_chars = text.chars().count(),
        );
        app.needs_redraw = true;
        return;
    };
    let Some(hunk) = file.hunks.get(key.hunk_idx) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_save_oob_hunk_idx",
            message = "save_active_input hit oob hunk_idx - body mutated mid-open?",
            outcome = "skipped",
            hunk_idx = key.hunk_idx,
            hunk_count = file.hunks.len(),
            lost_chars = text.chars().count(),
        );
        app.needs_redraw = true;
        return;
    };
    let Some(diff_line) = hunk.lines.get(key.line_idx) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_save_oob_line_idx",
            message = "save_active_input hit oob line_idx - body mutated mid-open?",
            outcome = "skipped",
            line_idx = key.line_idx,
            line_count = hunk.lines.len(),
            lost_chars = text.chars().count(),
        );
        app.needs_redraw = true;
        return;
    };
    let line_no = match diff_line.kind {
        DiffLineKind::Removed => diff_line.old_line,
        DiffLineKind::Added | DiffLineKind::Context => diff_line.new_line,
    }
    .unwrap_or(0);
    // Anchor on the single clicked line, not the whole hunk. A
    // comment on a brand-new file would otherwise capture the entire
    // file (Added hunks span the file body); now the captured context
    // stays compact and the agent gets precise per-line context.
    let commit = overlay.current_commit_sha();
    // Snapshot everything off the anchored line into owned locals so the
    // `overlay.files` borrows drop before the comment is pushed.
    let target = overlay.target.clone();
    let path = file.path.clone();
    let side = anchor_side(diff_line.kind);
    let content_hash = resolver::anchor_hash(&diff_line.text);
    let context = resolver::capture_context(hunk, key.line_idx, CONTEXT_RADIUS);
    let prior_thread = input.prior_comment.as_ref().map(|c| c.thread.clone());
    // Every scope persists a durable thread; `commit` records the scope
    // (the current sha, or `None` in whole-diff). Editing an existing chip
    // reuses the prior thread's identity + comment chain.
    let anchor = ReviewAnchor {
        path: path.clone(),
        side,
        line: line_no,
        content_hash,
        context,
        base_ref: target,
    };
    // A thread's home is the scope it was authored in, so only a new one
    // takes the scope being saved from; an edit keeps whatever it has.
    let is_new = prior_thread.is_none();
    let home = is_new || prior_thread.as_ref().is_some_and(|t| t.commit == commit);
    let mut thread = build_thread(prior_thread, anchor, &text, input.edit_turn, home);
    if is_new {
        thread.commit.clone_from(&commit);
    }
    // The chip snippet / editor fallback mirror the first user turn, which
    // stays stable whether this save edited a later turn or appended a reply.
    let comment_text = thread
        .comments
        .iter()
        .find(|c| matches!(c.author, ReviewAuthor::User))
        .map_or_else(|| text.clone(), |c| c.text.clone());
    // Persist FIRST so `persisted` reflects a confirmed write. A durable
    // comment whose write is skipped (no branch / project / store) or
    // fails stays at-risk - view.rs counts it as droppable - rather than
    // being marked durable on scope alone.
    let persisted = if let (Some(project), Some(branch), Some(workspace)) =
        (project.as_deref(), branch.as_deref(), workspace.as_ref())
    {
        let (respond_tx, mut respond_rx) = tokio::sync::oneshot::channel();
        let dispatched = workspace.dispatch(forge_workspace::Command::UpsertReviewThread {
            project: project.to_owned(),
            branch: branch.to_owned(),
            thread: thread.clone(),
            respond: respond_tx,
        });
        dispatched.ok().and_then(|()| respond_rx.try_recv().ok()).unwrap_or_else(|| {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_review_thread_not_persisted",
                message = "review comment persistence unconfirmed on the bus; kept in-memory only",
                outcome = "at_risk",
            );
            false
        })
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_review_thread_not_persisted",
            message = "review comment could not be persisted (no branch / project / store); kept in-memory only",
            outcome = "at_risk",
            has_branch = branch.is_some(),
            has_project = project.is_some(),
        );
        false
    };
    let comment = HunkComment {
        key,
        path,
        line: line_no,
        comment_text,
        commit,
        thread,
        authored_this_session: true,
        anchor_note: None,
        persisted,
    };
    // No dedup here: the card's lifetime while its editor is open belongs
    // to `reopen_comment_for_turn`, which takes it out of `comments` and
    // parks it, and puts it back if the edit is abandoned. So a save is
    // always an addition.
    overlay.comments.push(comment);
    overlay.recompute_comment_counts();
    app.needs_redraw = true;
}

/// Map a diff line's kind to the review side its line number lives on:
/// removed lines are the old side, added / context lines the new side.
fn anchor_side(kind: DiffLineKind) -> ReviewSide {
    match kind {
        DiffLineKind::Removed => ReviewSide::Old,
        DiffLineKind::Added | DiffLineKind::Context => ReviewSide::New,
    }
}

/// Build (or update) a durable [`ReviewThread`] for a review comment.
/// Reuses `prior`'s id / status / timestamps and comment chain:
/// `edit_turn = Some(idx)` rewrites that turn in place (only a
/// `User`-authored turn in range; an agent turn or out-of-range index
/// is left untouched), and `edit_turn = None` appends a new user turn
/// as a reply. Mints a fresh Open thread when there is no prior; the
/// caller stamps the scope `commit`. The store stamps `created_at` /
/// `updated_at` and any empty comment `at` on write, so they start
/// empty here.
pub(super) fn build_thread(
    prior: Option<forge_primitives::ReviewThread>,
    anchor: ReviewAnchor,
    text: &str,
    edit_turn: Option<usize>,
    home: bool,
) -> forge_primitives::ReviewThread {
    match prior {
        Some(mut thread) => {
            // The anchor is built from the view being saved from, and
            // every field of it - line, side, hash, context - is that
            // view's account of the code. Only the thread's own view may
            // replace it; from anywhere else the reply is a new turn on a
            // thread that has not moved.
            if home {
                thread.anchor = anchor;
            }
            match edit_turn {
                // Rewrite the targeted turn in place; an agent turn or an
                // out-of-range index is rejected - warn so the dropped text
                // is observable (unreachable via the UI, which only offers
                // your own turns as edit targets).
                Some(idx) => {
                    let turn_count = thread.comments.len();
                    let editable = thread
                        .comments
                        .get(idx)
                        .is_some_and(|c| matches!(c.author, ReviewAuthor::User));
                    if editable {
                        if let Some(turn) = thread.comments.get_mut(idx) {
                            text.clone_into(&mut turn.text);
                        }
                    } else {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "diff_overlay_edit_turn_rejected",
                            message = "edit targeted a non-editable turn (agent or out-of-range) - text dropped",
                            outcome = "skipped",
                            turn_idx = idx,
                            turn_count,
                            lost_chars = text.chars().count(),
                        );
                    }
                }
                // Reply: append the user's text as a new turn.
                None => thread.comments.push(ReviewComment {
                    author: ReviewAuthor::User,
                    text: text.to_owned(),
                    at: String::new(),
                    review_id: None,
                }),
            }
            thread
        }
        None => forge_primitives::ReviewThread {
            id: uuid::Uuid::new_v4().to_string(),
            anchor,
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: String::new(),
            updated_at: String::new(),
            commit: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diff_overlay::mouse::{handle_left_click, reopen_comment_for_turn};
    use crate::app::diff_overlay::reviews::session_work_pending;
    use crate::app::diff_overlay::test_support::*;
    use crate::app::diff_overlay::threads::{hydrate_threads, thread_in_scope};
    use crate::app::diff_overlay::types::{
        ActiveCommentInput, BodyRowKey, CachedScan, CommentRef, DiffScope, LineKey,
    };
    use crate::app::input::InputState;
    use crate::app::view::{ActiveView, set_active_view};
    use forge_workspace::env::git_diff::hunks::{DiffLine, FileHunks, FileStatus};
    use std::path::PathBuf;

    #[test]
    fn reopen_then_cancel_restores_saved_comment() {
        // F1 fix: clicking a chip stashes the saved comment on
        // active_input.prior_comment; Esc-cancel must restore it
        // to overlay.comments so a misclick + reflex Esc doesn't
        // destroy review notes.
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "I want to keep this".into(),
            commit: None,
            thread: user_thread("I want to keep this"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.recompute_comment_counts();
        state.body_keys =
            vec![BodyRowKey::CommentTurn { at: CommentRef { line: key, slot: 0 }, turn_idx: 0 }];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        // Click the chip → editor opens with prior_comment Some.
        let effect = handle_left_click(app.diff_overlay.as_mut().expect("overlay"), 60, 0, 160);
        assert!(effect.redraw);
        assert!(app.diff_overlay.as_ref().expect("overlay").active_input.is_some());
        assert!(
            app.diff_overlay
                .as_ref()
                .expect("overlay")
                .active_input
                .as_ref()
                .unwrap()
                .prior_comment
                .is_some(),
            "prior_comment stashed on chip reopen"
        );
        // Now press Esc → cancel_active_input restores prior.
        cancel_active_input(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay");
        assert!(after.active_input.is_none(), "editor closed");
        assert_eq!(after.comments.len(), 1, "saved comment restored");
        assert_eq!(after.comments[0].comment_text, "I want to keep this");
    }

    #[test]
    fn reopen_then_click_other_line_preserves_prior() {
        // F7: opening editor B via line click while editor A (a
        // chip reopen) is open must preserve A's prior_comment.
        let mut state = sample_state();
        let key_a = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key: key_a,
            path: "a.rs".into(),
            line: 1,
            comment_text: "saved".into(),
            commit: None,
            thread: user_thread("saved"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.recompute_comment_counts();
        // Body geometry: your-turn row at idx 0, hunk header at idx 1,
        // hunk line at idx 2.
        let key_b = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::CommentTurn { at: CommentRef { line: key_a, slot: 0 }, turn_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(key_b), right: Some(key_b) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Click chip → editor opens with prior Some.
        let _ = handle_left_click(&mut state, 60, 0, 160);
        assert!(state.active_input.as_ref().unwrap().prior_comment.is_some());
        assert_eq!(state.comments.len(), 0, "comment moved into prior");
        // Click a different diff line → editor B opens; A's prior
        // must have been restored to overlay.comments.
        let _ = handle_left_click(&mut state, 60, 2, 160);
        assert_eq!(state.active_input.as_ref().unwrap().key, key_b);
        assert_eq!(state.comments.len(), 1, "A's prior restored before B opens");
        assert_eq!(state.comments[0].comment_text, "saved");
    }

    #[test]
    fn reopen_chip_then_click_other_chip_preserves_both() {
        let mut state = sample_state();
        let key_a = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let key_b = LineKey { file_idx: 0, hunk_idx: 1, line_idx: 0 };
        state.comments.push(HunkComment {
            key: key_a,
            path: "a.rs".into(),
            line: 1,
            comment_text: "A".into(),
            commit: None,
            thread: user_thread("A"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.comments.push(HunkComment {
            key: key_b,
            path: "a.rs".into(),
            line: 5,
            comment_text: "B".into(),
            commit: None,
            thread: user_thread("B"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.recompute_comment_counts();
        state.body_keys = vec![
            BodyRowKey::CommentTurn { at: CommentRef { line: key_a, slot: 0 }, turn_idx: 0 },
            BodyRowKey::CommentTurn { at: CommentRef { line: key_b, slot: 0 }, turn_idx: 0 },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let _ = handle_left_click(&mut state, 60, 0, 160);
        let _ = handle_left_click(&mut state, 60, 1, 160);
        // Now editor is open on B with B as prior; A should be back
        // in overlay.comments.
        assert_eq!(state.active_input.as_ref().unwrap().key, key_b);
        assert_eq!(state.comments.len(), 1, "A restored, B in prior");
        assert_eq!(state.comments[0].key, key_a);
    }

    #[test]
    fn rail_switch_preserves_prior_comment() {
        let mut state = sample_state();
        let key_a = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key: key_a,
            path: "a.rs".into(),
            line: 1,
            comment_text: "A".into(),
            commit: None,
            thread: user_thread("A"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.recompute_comment_counts();
        state.body_keys =
            vec![BodyRowKey::CommentTurn { at: CommentRef { line: key_a, slot: 0 }, turn_idx: 0 }];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Reopen chip A.
        let _ = handle_left_click(&mut state, 60, 0, 160);
        assert!(state.active_input.as_ref().unwrap().prior_comment.is_some());
        // Click file 1 in the rail (row 4 in sample geometry).
        let _ = handle_left_click(&mut state, 5, 4, 160);
        // Editor closed, A restored.
        assert!(state.active_input.is_none());
        assert_eq!(state.comments.len(), 1);
        assert_eq!(state.comments[0].key, key_a);
    }

    #[test]
    fn reopen_edit_then_cancel_drops_edits_and_restores_prior() {
        // F1: user reopens chip, types edits, then dismisses (Esc).
        // Per GitHub edit-modal semantics, the chip restores to its
        // pre-edit state - the typed-over changes are intentionally
        // dropped. Verify the prior is restored verbatim AND the
        // helper reports the divergence as abandoned chars (so the
        // central DEBUG log fires for telemetry).
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "original text".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        let mut editor = InputState::new();
        editor.insert_str("original text with user-typed edits");
        state.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior.clone()),
            edit_turn: Some(0),
        });
        let abandoned = close_active_input_preserving_prior(&mut state);
        assert!(abandoned > 0, "user's typed-over text counts as abandoned");
        assert_eq!(state.comments.len(), 1);
        assert_eq!(state.comments[0].comment_text, "original text", "prior restored verbatim");
    }

    #[test]
    fn reopen_no_edit_then_cancel_reports_zero_abandoned() {
        // F1 boundary: when the editor's content equals the prior
        // exactly (user reopened, didn't type), abandoned should be 0
        // - no telemetry log fires for "viewed and dismissed".
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "exactly this".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        let mut editor = InputState::new();
        editor.insert_str("exactly this");
        state.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior),
            edit_turn: Some(0),
        });
        let abandoned = close_active_input_preserving_prior(&mut state);
        assert_eq!(abandoned, 0, "no divergence → no abandoned text");
        assert_eq!(state.comments.len(), 1);
    }

    #[test]
    fn fresh_editor_close_reports_abandoned_chars() {
        // F2 sister test: a fresh-editor dismissal via any of the
        // helper-using paths surfaces the abandoned count.
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut editor = InputState::new();
        editor.insert_str("draft typed by user");
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
        let abandoned = close_active_input_preserving_prior(&mut state);
        assert_eq!(abandoned, "draft typed by user".chars().count());
        assert!(state.comments.is_empty(), "fresh editor's text is not saved");
    }

    #[test]
    fn save_empty_fresh_editor_creates_no_chip() {
        // F8: fresh editor (prior None) + Enter on blank text →
        // no chip, no comment.
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.active_input = Some(ActiveCommentInput {
            key,
            editor: InputState::new(),
            prior_comment: None,
            edit_turn: None,
        });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay still set");
        assert!(after.active_input.is_none(), "editor closed");
        assert!(after.comments.is_empty(), "no blank chip created");
    }

    #[test]
    fn save_empty_reopened_chip_deletes_saved_comment() {
        // Clearing the only user turn + Enter removes the whole card.
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "soon-to-be-deleted".into(),
            commit: None,
            thread: user_thread("soon-to-be-deleted"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        state.active_input = Some(ActiveCommentInput {
            key,
            editor: InputState::new(), // empty editor
            prior_comment: Some(prior),
            edit_turn: Some(0),
        });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay still set");
        assert!(after.active_input.is_none());
        assert!(after.comments.is_empty(), "clearing the only user turn deletes the card");
    }

    #[test]
    fn save_stamps_current_commit_sha_in_commit_mode() {
        use forge_workspace::env::git_diff::hunks::Hunk;
        let mut app = App::test_default();
        let file = FileHunks {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    text: "x".into(),
                    old_line: None,
                    new_line: Some(1),
                }],
            }],
        };
        let mut state =
            DiffOverlayState::new(PathBuf::from("/tmp"), "main".to_owned(), vec![file.clone()]);
        state.commits = vec![commit_meta("aaa", "s")];
        state.scope = DiffScope::Commit(0);
        state.commit_cache = vec![Some(CachedScan { files: vec![file], scanner_ok: true })];
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut editor = InputState::new();
        editor.insert_str("note");
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let o = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(o.comments.len(), 1);
        assert_eq!(o.comments[0].commit, Some("aaa".to_owned()), "commit sha stamped");
    }

    #[test]
    fn save_stamps_no_commit_in_whole_diff_mode() {
        let mut app = App::test_default();
        let file = one_file("a.rs", FileStatus::Modified);
        let mut file = file;
        file.hunks = vec![forge_workspace::env::git_diff::hunks::Hunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Added,
                text: "x".into(),
                old_line: None,
                new_line: Some(1),
            }],
        }];
        let mut state = DiffOverlayState::new(PathBuf::from("/tmp"), "HEAD".to_owned(), vec![file]);
        // Whole-diff-only mode: no commits, scope stays WholeDiff.
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut editor = InputState::new();
        editor.insert_str("note");
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let o = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(o.comments.len(), 1);
        assert_eq!(o.comments[0].commit, None, "whole-diff comments carry no commit");
    }

    // ---- key + mouse: commit navigation and the jump dropdown ----
    //
    // These drive cached navigation only (Ready outcomes) - the
    // NeedsScan → `spawn_local` glue needs a LocalSet runtime, and the
    // NeedsScan branch itself is covered by the state tests above.

    #[test]
    fn stock_thread_mints_a_distinct_id_per_call() {
        let first = stock_thread();
        let second = stock_thread();
        assert_ne!(first.id, second.id, "two stock threads are not one thread");
    }

    #[test]
    fn save_active_input_persists_a_whole_diff_thread() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(
            &mut overlay,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "needs a bound check",
        );
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let threads = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(threads.len(), 1, "the whole-diff comment persisted a thread");
        assert_eq!(threads[0].anchor.line, 10);
        assert_eq!(threads[0].anchor.side, ReviewSide::New);
        assert_eq!(threads[0].status, ReviewStatus::Open);
        assert_eq!(threads[0].commit, None, "a whole-diff thread carries no commit scope");
        assert_eq!(threads[0].comments[0].text, "needs a bound check");
        assert!(!threads[0].created_at.is_empty(), "store stamped created_at");
        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(
            comment.thread.commit, None,
            "the in-memory comment carries a whole-diff thread"
        );
    }

    #[test]
    fn save_active_input_persists_a_commit_scoped_thread() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("z", 3)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.scope = DiffScope::Commit(0);
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "commit note");
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let threads = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(threads.len(), 1, "the commit-scoped comment persisted a thread");
        assert_eq!(threads[0].commit.as_deref(), Some("aaa"), "the thread carries the commit sha");
        assert_eq!(threads[0].comments[0].text, "commit note");
        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert!(comment.persisted, "the in-memory comment is a confirmed durable write");
        assert_eq!(
            comment.thread.commit.as_deref(),
            Some("aaa"),
            "the in-memory comment's thread carries the commit sha",
        );
    }

    #[test]
    fn build_thread_rewrites_only_the_targeted_turn() {
        let mut prior = user_thread("a");
        prior.comments.push(agent_turn("x"));
        prior.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "c".to_owned(),
            at: String::new(),
            review_id: None,
        });
        let thread = build_thread(Some(prior), test_anchor(), "C!", Some(2), true);
        assert_eq!(thread.comments[0].text, "a", "the first turn is untouched");
        assert_eq!(thread.comments[1].text, "x", "the agent turn is untouched");
        assert_eq!(thread.comments[2].text, "C!", "only the targeted turn is rewritten");
    }

    #[test]
    fn build_thread_rejects_editing_an_agent_turn() {
        let mut prior = user_thread("a");
        prior.comments.push(agent_turn("x"));
        let thread = build_thread(Some(prior), test_anchor(), "hijack", Some(1), true);
        assert_eq!(thread.comments.len(), 2, "no turn is added on a rejected edit");
        assert_eq!(thread.comments[1].text, "x", "an agent turn is not editable");
    }

    #[test]
    fn build_thread_appends_a_reply_when_edit_turn_is_none() {
        let mut prior = user_thread("a");
        prior.comments.push(agent_turn("x"));
        let thread = build_thread(Some(prior), test_anchor(), "thanks", None, true);
        assert_eq!(thread.comments.len(), 3, "a reply appends a new turn");
        assert!(matches!(thread.comments[2].author, ReviewAuthor::User));
        assert_eq!(thread.comments[2].text, "thanks");
    }

    #[test]
    fn build_thread_mints_a_fresh_thread_without_a_prior() {
        let thread = build_thread(None, test_anchor(), "new note", None, true);
        assert_eq!(thread.comments.len(), 1);
        assert!(matches!(thread.comments[0].author, ReviewAuthor::User));
        assert_eq!(thread.comments[0].text, "new note");
        assert_eq!(thread.status, ReviewStatus::Open);
    }

    #[test]
    fn save_edit_turn_rewrites_that_turn_only() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut prior_thread = user_thread("first note");
        prior_thread.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "second note".to_owned(),
            at: String::new(),
            review_id: None,
        });
        let prior = HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "first note".into(),
            commit: None,
            thread: prior_thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        };
        let mut editor = InputState::new();
        editor.insert_str("second note EDITED");
        overlay.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior),
            edit_turn: Some(1),
        });
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(comment.thread.comments[0].text, "first note", "turn 0 is untouched");
        assert_eq!(comment.thread.comments[1].text, "second note EDITED", "turn 1 was rewritten");
        assert_eq!(comment.comment_text, "first note", "the snippet still mirrors the first turn");
    }

    #[test]
    fn save_edit_turn_persists_through_redb_keeping_the_agent_reply() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut thread = user_thread("first");
        thread.id = "t-e2e".to_owned();
        thread.comments = vec![
            ReviewComment {
                author: ReviewAuthor::User,
                text: "first".into(),
                at: String::new(),
                review_id: None,
            },
            agent_turn("addressed"),
            ReviewComment {
                author: ReviewAuthor::User,
                text: "third".into(),
                at: String::new(),
                review_id: None,
            },
        ];
        ws.upsert_review_thread("forge", "feat", thread.clone());
        let prior = HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "first".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        };
        let mut editor = InputState::new();
        editor.insert_str("third EDITED");
        overlay.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior),
            edit_turn: Some(2),
        });
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let threads = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(threads.len(), 1);
        let t = &threads[0];
        assert_eq!(t.comments.len(), 3, "the chain length is preserved through the reload");
        assert_eq!(t.comments[0].text, "first", "turn 0 intact");
        assert!(
            matches!(t.comments[1].author, ReviewAuthor::Agent { .. }),
            "the interleaved agent reply survived",
        );
        assert_eq!(t.comments[1].text, "addressed", "the agent reply text is intact");
        assert_eq!(t.comments[2].text, "third EDITED", "turn 2 was rewritten");
        let c = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(c.comment_text, "first", "comment_text mirrors the first user turn");
    }

    #[test]
    fn clearing_a_middle_turn_trims_it_and_keeps_the_thread() {
        let (mut app, ws, _dir) = clear_turn_setup(
            vec![
                ReviewComment {
                    author: ReviewAuthor::User,
                    text: "first".into(),
                    at: String::new(),
                    review_id: None,
                },
                agent_turn("reply"),
                ReviewComment {
                    author: ReviewAuthor::User,
                    text: "third".into(),
                    at: String::new(),
                    review_id: None,
                },
            ],
            2,
        );

        save_active_input(&mut app);

        let o = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(o.comments.len(), 1, "the card survives");
        let c = &o.comments[0];
        assert_eq!(c.thread.comments.len(), 2, "only the cleared turn was removed");
        assert_eq!(c.thread.comments[0].text, "first");
        assert!(
            matches!(c.thread.comments[1].author, ReviewAuthor::Agent { .. }),
            "the agent reply survives",
        );
        assert_eq!(c.comment_text, "first", "comment_text still mirrors the first user turn");
        let threads = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(threads.len(), 1, "the thread survives in redb");
        assert_eq!(threads[0].comments.len(), 2, "redb thread trimmed to two turns");
    }

    #[test]
    fn clearing_the_last_user_turn_deletes_the_whole_thread() {
        let (mut app, ws, _dir) = clear_turn_setup(
            vec![
                ReviewComment {
                    author: ReviewAuthor::User,
                    text: "only".into(),
                    at: String::new(),
                    review_id: None,
                },
                agent_turn("reply"),
            ],
            0,
        );

        save_active_input(&mut app);

        let o = app.diff_overlay.as_ref().expect("overlay");
        assert!(o.comments.is_empty(), "no user turn remains, so the card is gone");
        assert!(
            ws.load_review_threads("forge", "feat").expect("load").is_empty(),
            "an orphaned agent reply is not left behind in redb",
        );
    }

    #[test]
    fn reply_appends_a_new_user_turn_without_changing_state() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "first note".into(),
            commit: None,
            thread: user_thread("first note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        };
        let mut editor = InputState::new();
        editor.insert_str("second thought");
        overlay.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: Some(prior), edit_turn: None });
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(comment.thread.comments.len(), 2, "the reply appended a turn");
        assert_eq!(comment.thread.comments[0].text, "first note");
        assert_eq!(comment.thread.comments[1].text, "second thought");
        assert!(matches!(comment.thread.comments[1].author, ReviewAuthor::User));
        assert_eq!(comment.thread.status, ReviewStatus::Open, "a reply never changes state");
    }

    #[test]
    fn a_second_reply_appends_a_second_turn() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "note".into(),
            commit: None,
            thread: user_thread("note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        for reply in ["one", "two"] {
            if let Some(o) = app.diff_overlay.as_mut() {
                reopen_comment_for_turn(o, CommentRef { line: key, slot: 0 }, None);
                if let Some(input) = o.active_input.as_mut() {
                    input.editor.insert_str(reply);
                }
            }
            save_active_input(&mut app);
        }

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        let texts: Vec<&str> = comment.thread.comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["note", "one", "two"], "each reply appends another turn");
    }

    #[test]
    fn empty_reply_restores_the_thread_untouched() {
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "keep me".into(),
            commit: None,
            thread: user_thread("keep me"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        state.active_input = Some(ActiveCommentInput {
            key,
            editor: InputState::new(),
            prior_comment: Some(prior),
            edit_turn: None,
        });
        app.diff_overlay = Some(state);

        save_active_input(&mut app);

        let after = app.diff_overlay.as_ref().expect("overlay");
        assert!(after.active_input.is_none());
        assert_eq!(after.comments.len(), 1, "an empty reply restores the comment");
        assert_eq!(after.comments[0].thread.comments.len(), 1, "no empty turn appended");
        assert_eq!(after.comments[0].comment_text, "keep me");
    }

    #[test]
    fn saving_a_comment_leaves_the_other_cards_on_that_line_alone() {
        // The whole diff stacks a line's threads, so saving onto a line
        // that already carries one must replace that thread and nothing
        // else. Dropping the neighbours makes them vanish until the next
        // hydrate reinstates them from the store.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        for id in ["neighbour-a", "neighbour-b"] {
            let mut thread = stock_thread();
            thread.id = id.to_owned();
            overlay.comments.push(HunkComment {
                key,
                path: "src/x.rs".into(),
                line: 5,
                comment_text: id.into(),
                commit: None,
                thread,
                authored_this_session: false,
                anchor_note: None,
                persisted: true,
            });
        }
        with_editor(&mut overlay, key, "a third on the same line");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let mut ids: Vec<&str> = overlay.comments.iter().map(|c| c.thread.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids.iter().filter(|id| id.starts_with("neighbour")).count(),
            2,
            "both co-located cards survive a save on their line; got {ids:?}",
        );
        assert_eq!(overlay.comments.len(), 3, "and the new one joins them rather than replacing");
    }

    #[test]
    fn editing_a_comment_replaces_that_thread_rather_than_adding_one() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "first draft");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        let id = app.diff_overlay.as_ref().expect("overlay").comments[0].thread.id.clone();

        let overlay = app.diff_overlay.as_mut().expect("overlay");
        reopen_comment_for_turn(overlay, CommentRef { line: key, slot: 0 }, Some(0));
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor.insert_str("second draft");
        }
        save_active_input(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(overlay.comments.len(), 1, "an edit replaces its own card");
        assert_eq!(overlay.comments[0].thread.id, id, "and keeps the thread's identity");
    }

    #[test]
    fn saving_in_one_scope_keeps_the_same_threads_card_in_the_other() {
        // A thread authored on a commit is in scope for that commit AND
        // for the whole diff, and `hydrate_threads` deliberately keeps
        // both cards. Replacing by identity alone takes the other scope's
        // card with it, and a cached scope switch never re-hydrates, so
        // the comment is gone from the whole diff for the rest of the
        // overlay session.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files.clone());
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.commit_cache = vec![Some(CachedScan { files, scanner_ok: true })];
        overlay.scope = DiffScope::Commit(0);
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut card = |commit: Option<&str>| {
            let mut thread = stock_thread();
            thread.id = "shared".to_owned();
            thread.commit = Some("aaa".to_owned());
            overlay.comments.push(HunkComment {
                key,
                path: "src/x.rs".into(),
                line: 5,
                comment_text: "shared".into(),
                commit: commit.map(str::to_owned),
                thread,
                authored_this_session: false,
                anchor_note: None,
                persisted: true,
            });
        };
        card(None);
        card(Some("aaa"));

        reopen_comment_for_turn(&mut overlay, CommentRef { line: key, slot: 0 }, None);
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor.insert_str("still not right");
        }
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let scopes: Vec<Option<&str>> =
            overlay.comments.iter().map(|c| c.commit.as_deref()).collect();
        assert!(
            scopes.contains(&None),
            "the whole diff's card for this thread survives a save made in the commit's view; got {scopes:?}",
        );
        assert_eq!(
            overlay.comments.iter().filter(|c| c.commit.as_deref() == Some("aaa")).count(),
            1,
            "and the saved scope still holds exactly one card for it",
        );
    }

    #[test]
    fn replying_from_the_whole_diff_leaves_a_thread_in_its_own_commit() {
        // A thread's `commit` is where it was authored, not the view you
        // are looking at. The whole diff shows commit-homed threads, so
        // restamping it on save would evict the thread from its own
        // commit's view - durably, since the save persists it.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.scope = DiffScope::WholeDiff;
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let thread = cross_numbered_thread();
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 5,
            comment_text: "why this cast?".into(),
            commit: None,
            thread,
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });

        reopen_comment_for_turn(&mut overlay, CommentRef { line: key, slot: 0 }, None);
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor.insert_str("still not right");
        }
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let stored = ws.load_review_threads("forge", "feat").expect("load");
        let homed = stored.iter().find(|t| t.id == "homed").expect("thread persisted");
        assert_eq!(
            homed.commit.as_deref(),
            Some("aaa"),
            "the thread stays homed on the commit it was authored against",
        );
        assert!(
            thread_in_scope(homed, Some("aaa"), "main"),
            "so it still renders in that commit's own view",
        );
        assert_eq!(
            homed.anchor.line, 41,
            "and still points at the line its own view numbers it, not the one this view does",
        );
    }

    #[test]
    fn a_comment_authored_in_a_commit_is_homed_there() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files.clone());
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.commit_cache = vec![Some(CachedScan { files, scanner_ok: true })];
        overlay.scope = DiffScope::Commit(0);
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "a fresh comment here");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let stored = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(
            stored[0].commit.as_deref(),
            Some("aaa"),
            "a new thread takes the scope it was authored in as its home",
        );
    }

    #[test]
    fn saved_thread_survives_overlay_drop() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(
            &mut overlay,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "durable note",
        );
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        // The overlay drops (close / session-swap force-clear); redb keeps the thread.
        app.diff_overlay = None;
        let ws = app.workspace.clone().expect("ws");
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load").len(),
            1,
            "the thread outlives the overlay"
        );
    }

    #[test]
    fn empty_delete_removes_the_durable_thread_from_redb() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "delete me");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        let ws = app.workspace.clone().expect("ws");
        assert_eq!(ws.load_review_threads("forge", "feat").expect("load").len(), 1, "saved");

        // Reopen the chip, clear the text, save empty -> delete.
        if let Some(o) = app.diff_overlay.as_mut() {
            reopen_comment_for_turn(o, CommentRef { line: key, slot: 0 }, Some(0));
            if let Some(input) = o.active_input.as_mut() {
                input.editor = InputState::new();
            }
        }
        save_active_input(&mut app);

        assert!(
            ws.load_review_threads("forge", "feat").expect("load").is_empty(),
            "delete removed it from redb"
        );
        // A subsequent hydrate must not resurrect it.
        hydrate_threads(&mut app);
        assert!(app.diff_overlay.as_ref().expect("overlay").comments.is_empty(), "not resurrected");
    }

    #[test]
    fn unpersistable_whole_diff_comment_stays_at_risk() {
        // No branch (detached HEAD): the write is skipped, so the comment
        // is authored-this-session but NOT persisted - view.rs must count
        // it as droppable, not log a false "durable" success.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = None;
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert!(comment.authored_this_session);
        assert!(!comment.persisted, "no branch -> write skipped -> at risk");
        assert_eq!(comment.thread.commit, None, "still a whole-diff thread, just not durable");
    }

    #[test]
    fn save_leaves_a_different_thread_in_another_scope_alone() {
        // The save-path twin of the hydrate retain above. This is the
        // different-thread half; `saving_in_one_scope_keeps_the_same_
        // threads_card_in_the_other` covers the same thread rendered in
        // two scopes, which is the case a retain keyed on identity alone
        // gets wrong.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "on sha1".to_owned(),
            commit: Some("sha1".to_owned()),
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        let mut sibling = stock_thread();
        sibling.id = "sibling".to_owned();
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "another whole-diff thread".to_owned(),
            commit: None,
            thread: sibling,
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        with_editor(&mut overlay, key, "fresh whole-diff");
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let other = comments.iter().find(|c| c.commit.as_deref() == Some("sha1"));
        assert_eq!(
            other.map(|c| c.comment_text.as_str()),
            Some("on sha1"),
            "the commit-scoped comment at the same key survives a whole-diff save",
        );
        let whole: Vec<&str> = comments
            .iter()
            .filter(|c| c.commit.is_none())
            .map(|c| c.comment_text.as_str())
            .collect();
        assert_eq!(
            whole,
            vec!["another whole-diff thread", "fresh whole-diff"],
            "the save adds its own card without disturbing the thread beside it",
        );
    }

    #[test]
    fn write_failure_with_all_present_keeps_comment_at_risk() {
        // Workspace + project + branch all present, but its store isn't
        // open, so upsert returns false - the comment must stay at-risk
        // (persisted = false), not be marked durable on scope alone.
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);
        // Deliberately NO install_db_for_test: the write will fail.
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert!(comment.authored_this_session);
        assert!(!comment.persisted, "a failed write with all present stays at-risk");
        assert_eq!(comment.thread.commit, None, "still a whole-diff thread");
    }

    #[test]
    fn reopen_then_cancel_keeps_a_hydrated_chip_non_actionable() {
        // A read-only view of a prior review (hydrated threads) that the
        // user clicks then Esc-cancels must not become session-authored,
        // so closing the overlay re-prompts the agent with nothing.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "h".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 10,
                    content_hash: resolver::content_hash("keep"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "prior".to_owned(),
                    at: "t0".to_owned(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: None,
            }],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("keep", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let key = app.diff_overlay.as_ref().expect("overlay").comments[0].key;
        if let Some(o) = app.diff_overlay.as_mut() {
            reopen_comment_for_turn(o, CommentRef { line: key, slot: 0 }, Some(0));
        }
        cancel_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert!(!comment.authored_this_session, "reopen + cancel keeps the chip hydrated");
        assert!(!session_work_pending(&app), "and never nudges the agent");
    }

    #[test]
    fn force_clear_keeps_persisted_threads_in_redb() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        app.active_view = crate::app::view::ActiveView::Diff;

        // A session-swap force-clear drops the overlay without going
        // through close_with_submit; the persisted thread must survive.
        crate::app::view::set_active_view(&mut app, crate::app::view::ActiveView::Launchpad);
        assert!(app.diff_overlay.is_none(), "overlay force-cleared");
        let ws = app.workspace.clone().expect("ws");
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load").len(),
            1,
            "the persisted thread survives the force-clear",
        );
    }
}
