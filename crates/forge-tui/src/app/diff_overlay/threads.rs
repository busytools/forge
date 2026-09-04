//! Persisted review threads as the overlay sees them: loading a
//! branch's threads and re-anchoring them onto a fresh scan, the
//! fallback placement for outdated threads, the resolve / reopen
//! card actions, and the replies-waiting tally parked on the session.

use super::types::{AnchorNote, CommentRef, HunkComment, LineKey, ThreadAction};
use crate::app::App;
use forge_primitives::review::{ReviewAuthor, ReviewSide, ReviewStatus, ReviewThread};
use forge_workspace::env::git_diff::hunks::FileHunks;
use forge_workspace::env::git_diff::resolver::{self, AnchorResolution};

/// Placement for an outdated thread whose exact line may be gone,
/// avoiding any `occupied` key so it never lands on a line a live thread
/// already holds. Preference: the same line number on the same side,
/// else the nearest FREE surviving line in the file, else the file's
/// first free line, else the document's first free line; only when no
/// free line remains does it stack on the nearest occupied line. Returns
/// the key plus the anchored line's context (empty on a fallback line).
/// `None` only when the diff has no lines at all.
fn outdated_placement(
    files: &[FileHunks],
    path: &str,
    side: ReviewSide,
    line: u32,
    occupied: &std::collections::HashSet<LineKey>,
) -> Option<LineKey> {
    if let Some(file_idx) = files.iter().position(|f| f.path == path) {
        // Same-side candidates, nearest first (stable, so equal distances
        // keep document order).
        let mut candidates: Vec<(u32, LineKey)> = Vec::new();
        for (hunk_idx, hunk) in files[file_idx].hunks.iter().enumerate() {
            for (line_idx, diff_line) in hunk.lines.iter().enumerate() {
                let number = match side {
                    ReviewSide::Old => diff_line.old_line,
                    ReviewSide::New => diff_line.new_line,
                };
                if let Some(number) = number {
                    candidates
                        .push((number.abs_diff(line), LineKey { file_idx, hunk_idx, line_idx }));
                }
            }
        }
        candidates.sort_by_key(|(dist, _)| *dist);
        if let Some((_, key)) = candidates.iter().find(|(_, key)| !occupied.contains(key)) {
            return Some(*key);
        }
        // Same-side lines all taken: a free line anywhere in the file.
        if let Some(key) = first_free_line_in_file(&files[file_idx], file_idx, occupied) {
            return Some(key);
        }
        // Genuinely no free line in the file: stack on the nearest.
        if let Some((_, key)) = candidates.first() {
            return Some(*key);
        }
    }
    // File absent: the document's first free line, else stack on its first.
    first_free_line(files, occupied).or_else(|| first_line_key(files))
}

/// The first line's key in `file` not already in `occupied` (skipping
/// empty hunks), or `None` when every line is taken or absent.
fn first_free_line_in_file(
    file: &FileHunks,
    file_idx: usize,
    occupied: &std::collections::HashSet<LineKey>,
) -> Option<LineKey> {
    file.hunks.iter().enumerate().find_map(|(hunk_idx, hunk)| {
        (0..hunk.lines.len())
            .map(|line_idx| LineKey { file_idx, hunk_idx, line_idx })
            .find(|key| !occupied.contains(key))
    })
}

/// The first free line's key across the whole document.
fn first_free_line(
    files: &[FileHunks],
    occupied: &std::collections::HashSet<LineKey>,
) -> Option<LineKey> {
    files
        .iter()
        .enumerate()
        .find_map(|(file_idx, file)| first_free_line_in_file(file, file_idx, occupied))
}

/// The first line's key across the whole document, or `None` when the
/// diff has no lines. The last-resort stack anchor when no line is free.
fn first_line_key(files: &[FileHunks]) -> Option<LineKey> {
    files.iter().enumerate().find_map(|(file_idx, file)| {
        file.hunks
            .iter()
            .enumerate()
            .find(|(_, hunk)| !hunk.lines.is_empty())
            .map(|(hunk_idx, _)| LineKey { file_idx, hunk_idx, line_idx: 0 })
    })
}

/// The `LineKey` of the line in `file` whose number on `side` equals
/// `line`, or `None` when no such line is present. Used to re-anchor a
/// comment onto a file's hunks after they change.
pub(super) fn find_line_key(
    file: &FileHunks,
    file_idx: usize,
    side: ReviewSide,
    line: u32,
) -> Option<LineKey> {
    for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
        for (line_idx, diff_line) in hunk.lines.iter().enumerate() {
            let number = match side {
                ReviewSide::Old => diff_line.old_line,
                ReviewSide::New => diff_line.new_line,
            };
            if number == Some(line) {
                return Some(LineKey { file_idx, hunk_idx, line_idx });
            }
        }
    }
    None
}

/// The user-authored text of a thread (first user comment), for the
/// existing chip/box render path.
fn thread_text(thread: &forge_primitives::ReviewThread) -> String {
    thread
        .comments
        .iter()
        .find(|c| matches!(c.author, ReviewAuthor::User))
        .or_else(|| thread.comments.first())
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

/// Whether a stored thread renders in the scope now on screen. The whole
/// diff is a union over the branch, so a rewrite that erases the commit a
/// thread was authored against cannot put it out of reach; a commit's own
/// view takes only what was authored there.
///
/// Only the whole diff checks `base_ref`, because only it is numbered
/// against the target: a thread counted from another base would land on
/// unrelated code. A commit's diff is `sha^..sha`, whose line numbers do
/// not depend on the target at all, so filtering it by base would hide a
/// thread from the one view that can place it correctly.
pub(super) fn thread_in_scope(
    thread: &ReviewThread,
    scope_commit: Option<&str>,
    target: &str,
) -> bool {
    match scope_commit {
        Some(sha) => thread.commit.as_deref() == Some(sha),
        None => thread.anchor.base_ref == target,
    }
}

/// Load persisted review threads for the current scope (the active
/// commit's sha, or whole-diff when `None`), re-anchor each against the
/// fresh scan, and install them as the overlay's comments for that scope
/// (replacing the prior in-scope set, leaving other scopes' comments
/// untouched). Moved-line updates and drift-to-`Outdated` flips are
/// written back to redb. No-op without a workspace / project / branch.
pub(super) fn hydrate_threads(app: &mut App) {
    let project = app.active_session().and_then(|s| s.project.clone());
    let workspace = app.workspace.clone();
    let Some(overlay) = app.diff_overlay.as_mut() else {
        return;
    };
    let (Some(project), Some(branch), Some(workspace)) =
        (project, overlay.branch.clone(), workspace)
    else {
        return;
    };

    // Reviews are branch-global (scope-independent); refresh them here so
    // chip tags and the `l` list reflect what's on disk. A corrupt reviews
    // row surfaces the same "failed to load" banner as the threads path -
    // the `reviews` table is a separate row, so its failure is independent.
    match workspace.load_reviews(&project, &branch) {
        Ok(reviews) => overlay.reviews = reviews,
        Err(error) => {
            overlay.review_load_error = Some(error);
            app.needs_redraw = true;
            return;
        }
    }

    // Surface a load failure as a visible notice rather than a silent
    // empty pane; a successful load clears any prior notice.
    let loaded = match workspace.load_review_threads(&project, &branch) {
        Ok(threads) => {
            overlay.review_load_error = None;
            threads
        }
        Err(error) => {
            overlay.review_load_error = Some(error);
            app.needs_redraw = true;
            return;
        }
    };
    // Threads are keyed by (project, branch) across every scope; process
    // only those in the current scope (the active commit's sha, or
    // whole-diff threads against the current target), keeping the rest
    // untouched so the whole-row writeback below preserves them instead of
    // silently dropping other scopes' threads.
    let scope_commit = overlay.current_commit_sha();
    let target = overlay.target.clone();
    let (mine, others): (Vec<_>, Vec<_>) =
        loaded.into_iter().partition(|t| thread_in_scope(t, scope_commit.as_deref(), &target));
    let had_in_scope = overlay.comments.iter().any(|c| c.commit == scope_commit);
    if mine.is_empty() && !had_in_scope {
        // Nothing in scope to re-anchor, so `others` is already every
        // thread on the branch at its final status.
        park_replies_waiting(app, &branch, &others);
        return;
    }

    let mut rebuilt = Vec::with_capacity(mine.len());
    let mut persist = others;
    let mut changed = false;
    // Live (in-place / moved) threads claim their real line in pass 1;
    // outdated fallbacks fill the remaining free lines in pass 2, so an
    // outdated box never lands on a key a live thread already holds
    // (which would route a click / edit to the wrong thread).
    let mut occupied: std::collections::HashSet<LineKey> = std::collections::HashSet::new();
    let mut deferred_outdated = Vec::new();
    for mut thread in mine {
        // A thread's anchor line and its drift state are recorded in the
        // numbering of the scope it was authored in. Another view may
        // place the card, but the row it lands on there is not a fact
        // about the thread - so only its own view writes either back, or
        // reports it as having moved.
        let home = thread.commit.as_deref() == scope_commit.as_deref();
        let resolution = resolver::resolve_anchor(&thread.anchor, &overlay.files, home);
        match resolution {
            AnchorResolution::InPlace { file_idx, hunk_idx, line_idx }
            | AnchorResolution::Moved { file_idx, hunk_idx, line_idx, .. } => {
                let moved_from = match resolution {
                    AnchorResolution::Moved { from, .. } => Some(from),
                    _ => None,
                };
                let resolved = overlay
                    .files
                    .get(file_idx)
                    .and_then(|f| f.hunks.get(hunk_idx))
                    .and_then(|h| h.lines.get(line_idx));
                let line = resolved
                    .and_then(|dl| match thread.anchor.side {
                        ReviewSide::Old => dl.old_line,
                        ReviewSide::New => dl.new_line,
                    })
                    .unwrap_or(thread.anchor.line);
                if home {
                    if thread.anchor.line != line {
                        thread.anchor.line = line;
                        changed = true;
                    }
                    if thread.status == ReviewStatus::Outdated {
                        thread.status = ReviewStatus::Open;
                        changed = true;
                    }
                }
                let key = LineKey { file_idx, hunk_idx, line_idx };
                occupied.insert(key);
                rebuilt.push(HunkComment {
                    key,
                    path: thread.anchor.path.clone(),
                    line,
                    comment_text: thread_text(&thread),
                    commit: scope_commit.clone(),
                    thread: thread.clone(),
                    authored_this_session: false,
                    // Only its own view can say where it came from: another
                    // one never held it at the line it records.
                    anchor_note: moved_from.filter(|_| home).map(|from| AnchorNote::Moved { from }),
                    persisted: true,
                });
                persist.push(thread);
            }
            AnchorResolution::Outdated(reason) => {
                if home && !matches!(thread.status, ReviewStatus::Resolved | ReviewStatus::Outdated)
                {
                    thread.status = ReviewStatus::Outdated;
                    changed = true;
                }
                deferred_outdated.push((thread, reason));
            }
        }
    }
    // Pass 2: place outdated threads on a surviving FREE line so they
    // render (yellow, against their captured context) without clobbering
    // a co-located live thread.
    for (thread, reason) in deferred_outdated {
        let Some(key) = outdated_placement(
            &overlay.files,
            &thread.anchor.path,
            thread.anchor.side,
            thread.anchor.line,
            &occupied,
        ) else {
            // Empty diff this open: keep the thread durable (it re-anchors
            // when the diff returns) but skip rendering.
            persist.push(thread);
            continue;
        };
        occupied.insert(key);
        rebuilt.push(HunkComment {
            key,
            path: thread.anchor.path.clone(),
            line: thread.anchor.line,
            comment_text: thread_text(&thread),
            commit: scope_commit.clone(),
            thread: thread.clone(),
            authored_this_session: false,
            // Said from any view: a card parked on a surviving line
            // without this reads as though it belongs there.
            anchor_note: Some(AnchorNote::Outdated(reason)),
            persisted: true,
        });
        persist.push(thread);
    }

    // The store knows nothing about this overlay session, so a rebuild
    // would reinstate a card the reviewer just wrote as if it had been
    // loaded - dropping it out of the review it should seal into while
    // still rendering it. Carry that state across, and keep a card the
    // store has never seen: its write was skipped or failed, so a
    // rebuild is not evidence that it is gone.
    let mut unwritten = Vec::new();
    for card in overlay.comments.iter().filter(|c| c.commit == scope_commit) {
        match rebuilt.iter_mut().find(|r| r.thread.id == card.thread.id) {
            // Only the session flag: a card the rebuild reinstates came
            // from the store, so it is durable by definition and the old
            // card's `persisted` can only contradict that.
            Some(fresh) => fresh.authored_this_session = card.authored_this_session,
            None if !card.persisted => unwritten.push(card.clone()),
            None => {}
        }
    }
    overlay.comments.retain(|c| c.commit != scope_commit);
    overlay.comments.extend(rebuilt);
    overlay.comments.extend(unwritten);
    overlay.recompute_comment_counts();
    if changed {
        let _ = workspace.dispatch(forge_workspace::Command::SaveReviewThreads {
            project: project.clone(),
            branch: branch.clone(),
            threads: persist.clone(),
        });
    }
    // `persist` is every thread on the branch, post re-anchoring - the
    // authoritative recompute that self-corrects a parked count drifted
    // from the store.
    park_replies_waiting(app, &branch, &persist);
    app.needs_redraw = true;
}

/// Park how many of `threads` still owe the reviewer a turn onto the
/// active session bucket, so the GIT badge and the NEEDS ATTENTION band
/// render from a field instead of querying the store per frame.
fn park_replies_waiting(app: &mut App, branch: &str, threads: &[ReviewThread]) {
    let count = threads.iter().filter(|t| t.awaits_reviewer()).count();
    if let Some(session) = app.try_active_bucket_mut() {
        session.review_replies_waiting = crate::app::ReviewRepliesWaiting::merge(
            session.review_replies_waiting.as_ref(),
            branch,
            count,
        );
    }
}

/// Re-park the waiting count after the reviewer mutated a thread. Reads
/// the store because the overlay only holds the current scope's threads;
/// safe on a keypress (the mutation itself already wrote), never on the
/// render path.
pub(super) fn refresh_replies_waiting(app: &mut App) {
    let Some(branch) = app.diff_overlay.as_ref().and_then(|o| o.branch.clone()) else {
        return;
    };
    let Some(project) = app.active_session().and_then(|s| s.project.clone()) else {
        return;
    };
    let Some(workspace) = app.workspace.clone() else {
        return;
    };
    let Ok(threads) = workspace.load_review_threads(&project, &branch) else {
        return;
    };
    park_replies_waiting(app, &branch, &threads);
}

/// Map a comment-button click to its transition and run it on the card
/// `at` names, so a click resolves exactly the one it landed on. A Reopen
/// that actually flips re-nudges the worker to take another look.
pub(super) fn apply_thread_action(app: &mut App, at: CommentRef, action: ThreadAction) {
    let (next, allowed_from): (ReviewStatus, &[ReviewStatus]) = match action {
        ThreadAction::Resolve => (
            ReviewStatus::Resolved,
            &[ReviewStatus::Open, ReviewStatus::Addressed, ReviewStatus::Outdated],
        ),
        ThreadAction::Reopen => {
            (ReviewStatus::Open, &[ReviewStatus::Addressed, ReviewStatus::Resolved])
        }
    };
    if set_thread_status_by_key(app, at, next, allowed_from) {
        if matches!(action, ThreadAction::Reopen) {
            renudge_reopened(app, at);
        }
        refresh_replies_waiting(app);
    }
}

/// Flip the thread the card at `at` carries to `next` when it is
/// currently in one of `allowed_from`, updating the in-memory card and
/// persisting the change. Returns whether it flipped. No-op (returns
/// `false`) when nothing is stacked there or its status isn't a legal
/// source.
fn set_thread_status_by_key(
    app: &mut App,
    at: CommentRef,
    next: ReviewStatus,
    allowed_from: &[ReviewStatus],
) -> bool {
    let project = app.active_session().and_then(|s| s.project.clone());
    let Some(overlay) = app.diff_overlay.as_mut() else {
        return false;
    };
    let Some(branch) = overlay.branch.clone() else {
        return false;
    };
    let Some(idx) = overlay.comment_index_at(at) else {
        return false;
    };
    let Some(thread) = overlay
        .comments
        .get_mut(idx)
        .map(|c| &mut c.thread)
        .filter(|t| allowed_from.contains(&t.status))
    else {
        return false;
    };
    thread.status = next;
    let id = thread.id.clone();
    if next != ReviewStatus::Resolved {
        // Expansion only means anything while a thread is collapsed by
        // default, and it is remembered per thread - so a thread that
        // leaves Resolved and comes back would otherwise return expanded
        // while every other resolved one is a marker.
        overlay.resolved_expanded.remove(&id);
    }
    // Entering or leaving Resolved swaps the card for a marker, so the
    // file's row count changed; clear its height like a collapse toggle.
    if let Some(slot) = overlay.measured_heights.get_mut(at.line.file_idx) {
        *slot = None;
    }
    app.needs_redraw = true;
    if let Some(project) = project
        && let Some(workspace) = app.workspace.as_ref()
    {
        let _ = workspace.dispatch(forge_workspace::Command::SetReviewThreadStatus {
            project,
            branch,
            thread_id: id,
            status: next,
        });
    }
    true
}

/// Nudge the worker after a comment is reopened, so it re-reads the review
/// and addresses the reopened point. Names the review number when the
/// reopened thread is filed. A no-op when there's no agent/session to
/// receive it (the flip + persist already happened).
fn renudge_reopened(app: &mut App, at: CommentRef) {
    if !app.has_active_agent() || app.session_id().is_none() {
        return;
    }
    let review_tag = app.diff_overlay.as_ref().and_then(|overlay| {
        // The latest round, not the origin: that is the exchange the
        // reviewer is unhappy with.
        let review_id =
            overlay.comments.get(overlay.comment_index_at(at)?)?.thread.latest_review()?;
        overlay.reviews.iter().find(|r| r.id == review_id).map(|r| r.number)
    });
    let nudge = match review_tag {
        Some(number) => format!(
            "Reopened a comment in review #{number} - take another look via the review MCP (`review__get`)."
        ),
        None => {
            "Reopened a review comment - take another look via the review MCP (`review__list`)."
                .to_owned()
        }
    };
    crate::app::input_submit::dispatch_review_nudge(app, nudge);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diff_overlay::comments::save_active_input;
    use crate::app::diff_overlay::keys::after_nav;
    use crate::app::diff_overlay::mouse::reopen_comment_for_turn;
    use crate::app::diff_overlay::state::DiffOverlayState;
    use crate::app::diff_overlay::test_support::*;
    use crate::app::diff_overlay::types::{ActiveCommentInput, CachedScan, DiffScope, NavOutcome};
    use crate::app::input::InputState;
    use forge_primitives::review::{ReviewAnchor, ReviewComment};
    use forge_workspace::env::git_diff::resolver::OutdatedReason;
    use std::path::PathBuf;

    #[test]
    fn whole_diff_takes_every_thread_and_a_commit_takes_only_its_own() {
        let mut whole = stock_thread();
        whole.commit = None;
        let mut on_aaa = stock_thread();
        on_aaa.commit = Some("aaa".to_owned());
        // Authored against a commit a force-push rewrote away: its sha
        // matches no entry in the rescanned commit list.
        let mut orphan = stock_thread();
        orphan.commit = Some("rewritten-away".to_owned());

        for thread in [&whole, &on_aaa, &orphan] {
            assert!(
                thread_in_scope(thread, None, "main"),
                "the whole diff is a union, so it takes every thread on the branch",
            );
        }

        assert!(thread_in_scope(&on_aaa, Some("aaa"), "main"));
        assert!(
            !thread_in_scope(&whole, Some("aaa"), "main"),
            "a whole-diff thread does not descend into an individual commit's view",
        );
        assert!(
            !thread_in_scope(&orphan, Some("aaa"), "main"),
            "a thread authored elsewhere is not this commit's",
        );
    }

    #[test]
    fn a_commit_scope_ignores_the_diff_base() {
        // `sha^..sha` is numbered against the commit's own parent, not the
        // target, so a thread authored under another base still places
        // correctly here. Filtering it out would hide it from the only
        // view that can.
        let mut thread = stock_thread();
        thread.commit = Some("aaa".to_owned());
        thread.anchor.base_ref = "HEAD".to_owned();
        assert!(
            thread_in_scope(&thread, Some("aaa"), "main"),
            "a commit takes its own threads whatever base the overlay was opened against",
        );
    }

    #[test]
    fn a_thread_against_another_diff_base_stays_out_of_the_union() {
        let mut thread = stock_thread();
        thread.anchor.base_ref = "HEAD".to_owned();
        assert!(
            !thread_in_scope(&thread, None, "main"),
            "line numbers against another base would anchor onto unrelated code",
        );
    }

    #[test]
    fn resolving_a_comment_makes_its_file_re_measure() {
        // Resolving folds the card to a marker, so the file loses rows
        // exactly as a collapse toggle does.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "rename tok to token");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        app.diff_overlay.as_mut().expect("overlay").measured_heights[0] = Some(40);

        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);

        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").measured_heights[0],
            None,
            "the file re-measures at its new row count, as a collapse toggle makes it",
        );
    }

    #[test]
    fn switching_back_to_a_cached_scope_rebuilds_its_cards_from_the_store() {
        // A thread rendered in two scopes is two cards, each owning its
        // own clone. Resolving through one leaves the other reading the
        // old status, and a cached scope switch installs files without a
        // scan - so nothing rebuilt the stale card for the rest of the
        // overlay session.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let ws = app.workspace.clone().expect("ws");
        let mut thread = stock_thread();
        thread.id = "shared".to_owned();
        thread.commit = Some("aaa".to_owned());
        thread.anchor = ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            line: 5,
            content_hash: resolver::anchor_hash("let a = 1;"),
            context: Vec::new(),
            base_ref: "main".to_owned(),
        };
        ws.save_review_threads("forge", "feat", &[thread]);

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files.clone());
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.commit_cache = vec![Some(CachedScan { files: files.clone(), scanner_ok: true })];
        overlay.whole_diff_cache = Some(CachedScan { files, scanner_ok: true });
        overlay.scope = DiffScope::WholeDiff;
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let outcome =
            app.diff_overlay.as_mut().expect("overlay").select_scope(DiffScope::Commit(0));
        assert_eq!(outcome, NavOutcome::Ready, "the commit's diff is cached");
        after_nav(&mut app, outcome);
        let line = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        apply_thread_action(&mut app, CommentRef { line, slot: 0 }, ThreadAction::Resolve);

        let outcome =
            app.diff_overlay.as_mut().expect("overlay").select_scope(DiffScope::WholeDiff);
        assert_eq!(outcome, NavOutcome::Ready, "and so is the whole diff");
        after_nav(&mut app, outcome);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let card = overlay
            .scoped_comments()
            .into_iter()
            .find(|c| c.thread.id == "shared")
            .expect("the whole diff still shows it");
        assert_eq!(
            card.thread.status,
            ReviewStatus::Resolved,
            "the card is rebuilt from the store, so it carries the status resolved elsewhere",
        );
        assert!(
            overlay.is_comment_collapsed(card),
            "and therefore collapses, which is the bug this PR fixes still live in the other view",
        );
    }

    #[test]
    fn a_rebuild_keeps_a_comment_whose_write_never_landed() {
        // `persisted: false` means the store never took it, so its
        // absence from a rebuild is not evidence that it is gone - it is
        // the only copy.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let mut thread = stock_thread();
        thread.id = "unwritten".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 5,
            comment_text: "the redb write failed".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        assert!(
            overlay.comments.iter().any(|c| c.thread.id == "unwritten"),
            "the only copy of an at-risk comment survives the rebuild",
        );
    }

    #[test]
    fn viewing_a_commits_thread_from_the_whole_diff_does_not_claim_it_moved() {
        // The two views number the same line differently. That is not a
        // move, and reporting one puts a confident false claim about
        // where the comment used to be on the card.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[cross_numbered_thread()]);
        app.diff_overlay = Some(cross_numbered_overlay());

        hydrate_threads(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let card = overlay.comments.iter().find(|c| c.thread.id == "homed").expect("card");
        assert_eq!(
            card.anchor_note, None,
            "this view never held the thread at its recorded line, so there is no origin \
             line it could truthfully report - absent is the only honest note here",
        );
    }

    #[test]
    fn switching_scopes_does_not_rewrite_a_threads_stored_anchor() {
        // The anchor line is recorded in the numbering of the scope the
        // thread was authored in. A view that counts differently may read
        // it, but writing to it makes the two views fight over the row.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[cross_numbered_thread()]);
        app.diff_overlay = Some(cross_numbered_overlay());
        hydrate_threads(&mut app);

        let stored_line = || {
            ws.load_review_threads("forge", "feat")
                .expect("load")
                .into_iter()
                .find(|t| t.id == "homed")
                .expect("thread")
                .anchor
                .line
        };
        assert_eq!(stored_line(), 41, "the whole diff must not renumber it");

        for scope in [DiffScope::Commit(0), DiffScope::WholeDiff, DiffScope::Commit(0)] {
            let outcome = app.diff_overlay.as_mut().expect("overlay").select_scope(scope);
            after_nav(&mut app, outcome);
            assert_eq!(stored_line(), 41, "and neither does stepping between them");
        }
    }

    #[test]
    fn a_view_that_cannot_place_a_thread_does_not_mark_it_outdated() {
        // A commit's line may be changed again by a later commit, so it
        // can be absent from the whole-branch diff while being perfectly
        // live in the commit that owns it. Recording that as drift there
        // makes one view's blind spot the thread's durable state.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[cross_numbered_thread()]);
        let mut overlay = cross_numbered_overlay();
        // The whole diff no longer carries that line at all.
        overlay.files = vec![single_hunk_file("src/x.rs", vec![added_line("let z = 9;", 5)])];
        overlay.whole_diff_cache =
            Some(CachedScan { files: overlay.files.clone(), scanner_ok: true });
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let stored = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(
            stored[0].status,
            ReviewStatus::Open,
            "the commit that owns it still shows it; this view just cannot see it",
        );
    }

    #[test]
    fn a_reply_from_another_view_does_not_make_the_commits_own_view_see_a_move() {
        // The second half of the same defect: once a foreign reply has
        // rewritten the anchor, the thread's own view finds its content
        // somewhere other than where the anchor now says, and reports a
        // move that never happened - which is the spurious note and the
        // redb writeback arriving through the save path instead.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[cross_numbered_thread()]);
        app.diff_overlay = Some(cross_numbered_overlay());
        hydrate_threads(&mut app);

        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        let overlay = app.diff_overlay.as_mut().expect("overlay");
        reopen_comment_for_turn(overlay, CommentRef { line: key, slot: 0 }, None);
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor.insert_str("one more thing");
        }
        save_active_input(&mut app);

        let outcome =
            app.diff_overlay.as_mut().expect("overlay").select_scope(DiffScope::Commit(0));
        after_nav(&mut app, outcome);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        // Scoped, not the whole list: the leftover whole-diff card is
        // still present here and is not what this view draws.
        let card = overlay
            .scoped_comments()
            .into_iter()
            .find(|c| c.thread.id == "homed")
            .expect("the commit draws its own card");
        assert_eq!(
            card.anchor_note, None,
            "the commit still holds the thread where it always did, so there is no move",
        );
    }

    #[test]
    fn editing_in_a_threads_own_view_refreshes_its_anchor() {
        // The other direction: its own view is the one that gets to say
        // where the thread now sits, so an edit there re-reads the line,
        // its context and its hash from what is on screen.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("fn wrapper() {", 4), added_line("let a = 1;", 5), added_line("}", 6)],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        let mut thread = stock_thread();
        thread.id = "own".to_owned();
        thread.commit = None;
        thread.anchor = ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            // Stale: nothing re-anchored it before this edit.
            line: 99,
            content_hash: 0,
            context: Vec::new(),
            base_ref: "main".to_owned(),
        };
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
            input.editor.insert_str("still unclear");
        }
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let stored = ws.load_review_threads("forge", "feat").expect("load");
        let own = stored.iter().find(|t| t.id == "own").expect("thread");
        assert_eq!(own.anchor.line, 5, "its own view re-reads where the line now is");
        assert_eq!(
            own.anchor.content_hash,
            resolver::anchor_hash("let a = 1;"),
            "and what is on it",
        );
    }

    #[test]
    fn a_view_that_cannot_place_a_thread_says_so_rather_than_parking_it_silently() {
        // Not placing a thread is a fact about THIS diff, not a claim
        // about another view's numbering - so every view may say it, and
        // must. Otherwise the card is parked on whatever line survived
        // nearby and reads as though it belongs there, which is the
        // failure the whole feature is built to avoid. A worker fixing
        // the commented line is exactly what removes it from the
        // whole-branch diff, so this is the main loop.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[cross_numbered_thread()]);
        let mut overlay = cross_numbered_overlay();
        overlay.files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("fn other() {", 4), added_line("    unrelated();", 5)],
        )];
        overlay.whole_diff_cache =
            Some(CachedScan { files: overlay.files.clone(), scanner_ok: true });
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let card = overlay
            .scoped_comments()
            .into_iter()
            .find(|c| c.thread.id == "homed")
            .expect("the card is still shown");
        assert_eq!(
            card.anchor_note,
            Some(AnchorNote::Outdated(OutdatedReason::Gone)),
            "this diff does not carry the line, and saying so is not a claim about any other view",
        );
    }

    #[test]
    fn an_at_risk_card_survives_entering_a_scope_that_has_threads() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        // The entered scope HAS a thread, so hydrate does not early-return.
        let mut homed = cross_numbered_thread();
        homed.id = "in-target-scope".to_owned();
        ws.save_review_threads("forge", "feat", &[homed]);
        let mut overlay = cross_numbered_overlay();
        // A whole-diff card whose redb write never landed: only copy.
        let mut lost = stock_thread();
        lost.id = "at-risk".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 5,
            comment_text: "write failed, this is the only copy".into(),
            commit: None,
            thread: lost,
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        app.diff_overlay = Some(overlay);

        let outcome =
            app.diff_overlay.as_mut().expect("overlay").select_scope(DiffScope::Commit(0));
        after_nav(&mut app, outcome);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        assert!(
            overlay.comments.iter().any(|c| c.thread.id == "at-risk"),
            "the only copy of an at-risk comment survives entering another scope",
        );
    }

    #[test]
    fn a_relocated_comment_announces_where_it_came_from() {
        // A comment that moves says so. Every other anchor-note assertion
        // here checks a note is ABSENT, so the producer could stop
        // emitting one and nothing would notice.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let mut thread = stock_thread();
        thread.id = "moved".to_owned();
        thread.commit = None;
        thread.anchor = ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            line: 41,
            content_hash: resolver::anchor_hash("let a = 1;"),
            context: vec!["fn wrapper() {".to_owned(), "}".to_owned()],
            base_ref: "main".to_owned(),
        };
        ws.save_review_threads("forge", "feat", &[thread]);
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("fn wrapper() {", 4), added_line("let a = 1;", 5), added_line("}", 6)],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let card = overlay.comments.iter().find(|c| c.thread.id == "moved").expect("card");
        assert_eq!(
            card.anchor_note,
            Some(AnchorNote::Moved { from: 41 }),
            "the code moved and the card names the line it left",
        );
    }

    #[test]
    fn hydrate_reanchors_in_place_moved_and_outdated() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed =
            |id: &str, line: u32, text: &str, context: &[&str]| forge_primitives::ReviewThread {
                id: id.to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line,
                    content_hash: resolver::anchor_hash(text),
                    context: context.iter().map(|c| (*c).to_owned()).collect(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: text.to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: None,
            };
        ws.save_review_threads(
            "forge",
            "feat",
            &[
                seed("keep", 5, "let a = 1;", &["inserted"]),
                // Its neighbours on both sides survive the insertion above.
                seed("move", 6, "let b = 2;", &["inserted2", "let c = renamed();"]),
                seed("changed", 20, "let c = 3;", &["let b = 2;"]),
                seed("vanished", 99, "let d = 4;", &["gone one", "gone two"]),
            ],
        );

        // Fresh scan: "let a = 1;" in place at 5; "let b = 2;" shifted to 8;
        // no "let c = 3;" anywhere (its content changed).
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![
                added_line("let a = 1;", 5),
                added_line("inserted", 6),
                added_line("inserted2", 7),
                added_line("let b = 2;", 8),
                added_line("let c = renamed();", 20),
            ],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let by_id =
            |id: &str| overlay.comments.iter().find(|c| c.thread.id == id).expect("comment for id");
        let keep = by_id("keep");
        assert_eq!(keep.key, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "in place");
        assert_eq!(keep.line, 5);
        let moved = by_id("move");
        assert_eq!(moved.key, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 3 }, "re-anchored");
        assert_eq!(moved.line, 8, "display line follows the move");
        // Content changed but the line number survives: placed inline
        // (line 20 = line_idx 4) and flagged Outdated.
        let changed = by_id("changed");
        assert_eq!(
            changed.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 4 },
            "inline outdated"
        );
        assert_eq!(changed.thread.status, ReviewStatus::Outdated);
        // Line number gone (99): the nearest line (20 = line_idx 4) is
        // already taken by "changed", so it falls to the next free line
        // (line_idx 2), still rendered and flagged Outdated.
        let vanished = by_id("vanished");
        assert_eq!(
            vanished.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 2 },
            "next free line"
        );
        assert_eq!(vanished.thread.status, ReviewStatus::Outdated);
        assert_ne!(vanished.key, changed.key, "outdated threads do not collide");

        // The move + outdated flips are written back to redb.
        let reloaded = ws.load_review_threads("forge", "feat").expect("load");
        let find = |id: &str| reloaded.iter().find(|t| t.id == id).expect("thread");
        assert_eq!(find("move").anchor.line, 8, "moved line persisted");
        assert_eq!(find("changed").status, ReviewStatus::Outdated, "outdated flip persisted");
        assert_eq!(find("vanished").status, ReviewStatus::Outdated, "outdated flip persisted");
        assert_eq!(find("keep").anchor.line, 5, "in-place line unchanged");
    }

    #[test]
    fn resolving_a_stacked_comment_acts_on_the_card_that_was_clicked() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, commit: Option<&str>| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 5,
                content_hash: resolver::content_hash("let a = 1;"),
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: id.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: commit.map(str::to_owned),
        };
        // Two threads on the same line: the whole diff stacks them.
        ws.save_review_threads("forge", "feat", &[seed("first", None), seed("second", Some("c0"))]);
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let line = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        apply_thread_action(&mut app, CommentRef { line, slot: 1 }, ThreadAction::Resolve);

        let stored = ws.load_review_threads("forge", "feat").expect("load");
        let status = |id: &str| stored.iter().find(|t| t.id == id).expect("thread").status;
        assert_eq!(
            status("second"),
            ReviewStatus::Resolved,
            "the second card's button resolves the second card's thread",
        );
        assert_eq!(
            status("first"),
            ReviewStatus::Open,
            "the card above it is untouched - resolving the wrong thread is the bad failure",
        );
    }

    #[test]
    fn a_force_push_orphan_renders_in_the_whole_diff() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let mut thread = stock_thread();
        thread.id = "orphan".to_owned();
        // The commit this was authored against no longer exists.
        thread.commit = Some("rewritten-away".to_owned());
        thread.anchor = ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            line: 5,
            content_hash: resolver::content_hash("let a = 1;"),
            context: Vec::new(),
            base_ref: "main".to_owned(),
        };
        ws.save_review_threads("forge", "feat", &[thread]);

        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.scope = DiffScope::WholeDiff;
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let orphan = overlay
            .comments
            .iter()
            .find(|c| c.thread.id == "orphan")
            .expect("the whole diff renders a comment whose commit was rewritten away");
        assert_eq!(
            orphan.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "re-anchored against the whole-diff scan, not its dead commit",
        );
        assert_eq!(orphan.thread.status, ReviewStatus::Open, "the line is still there");
        assert!(
            overlay.scoped_comments().iter().any(|c| c.thread.id == "orphan"),
            "and it survives the render-scope filter",
        );
    }

    #[test]
    fn outdated_placement_avoids_a_live_thread_key() {
        // A live thread holds line 10; an outdated thread whose content
        // was also at line 10 (now gone) must land on a DIFFERENT key so
        // clicking / editing one can't overwrite the other.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 10,
                content_hash: resolver::content_hash(text),
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: None,
        };
        ws.save_review_threads("forge", "feat", &[seed("live", "keep"), seed("stale", "old_body")]);
        // "keep" is live at line 10; "old_body" is gone.
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("keep", 10), added_line("neighbor", 11)],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let by_id = |id: &str| comments.iter().find(|c| c.thread.id == id).expect("comment");
        let live = by_id("live");
        let stale = by_id("stale");
        assert_eq!(
            live.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "live holds line 10"
        );
        assert_eq!(
            stale.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 },
            "outdated thread avoids the live key, taking the next free line",
        );
        assert_eq!(stale.thread.status, ReviewStatus::Outdated);
    }

    #[test]
    fn outdated_thread_with_absent_file_falls_back_to_document_start() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "gone".to_owned(),
                anchor: ReviewAnchor {
                    path: "removed.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: 1,
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "note".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: None,
            }],
        );
        // The commented file is no longer in the diff.
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("keep", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);
        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(
            comment.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "absent file falls back to the document's first line",
        );
        assert_eq!(comment.thread.status, ReviewStatus::Outdated);
    }

    #[test]
    fn comment_button_resolve_and_reopen_persist() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "needs a bound");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        let ws = app.workspace.clone().expect("ws");

        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);
        assert_eq!(thread_status(&app), ReviewStatus::Resolved, "in-memory resolves");
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load")[0].status,
            ReviewStatus::Resolved,
            "persisted"
        );

        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Reopen);
        assert_eq!(thread_status(&app), ReviewStatus::Open, "in-memory reopens");
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load")[0].status,
            ReviewStatus::Open,
            "persisted"
        );
    }

    #[test]
    fn reopen_flips_addressed_and_renudges_the_worker() {
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut overlay = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])],
        );
        overlay.branch = Some("feat".to_owned());
        overlay.reviews = vec![forge_primitives::ReviewSet {
            id: "rev".to_owned(),
            number: 1,
            summary: None,
            created_at: String::new(),
        }];
        let mut thread = filed_thread("rev");
        thread.status = ReviewStatus::Addressed;
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "look here".into(),
            commit: None,
            thread,
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Reopen);

        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").comments[0].thread.status,
            ReviewStatus::Open,
            "reopen flips an addressed thread back to open",
        );
        match rx.try_recv().expect("a re-nudge was dispatched") {
            forge_primitives::AgentCommand::PromptWithImages { text, .. } => {
                assert!(
                    text.contains("Reopened") && text.contains("review #1"),
                    "the re-nudge names the reopened review: {text}",
                );
            }
            other => panic!("expected PromptWithImages, got {other:?}"),
        }
    }

    #[test]
    fn hydrating_diff_recomputes_the_waiting_count_from_the_store() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[answered_thread("a")]);
        app.diff_overlay = Some(overlay_for_answered_threads());

        hydrate_threads(&mut app);

        assert_eq!(waiting_count(&app), Some(1), "an answered thread awaits the reviewer");
        assert_eq!(
            app.active_session()
                .and_then(|s| s.review_replies_waiting.as_ref())
                .map(|w| w.branch.clone()),
            Some("feat".to_owned()),
        );
    }

    /// Only a reviewer turn retires an answer. Opening `/diff` on some
    /// other branch must not take one branch's empty result as licence
    /// to drop another branch's live count.
    #[test]
    fn hydrating_another_branch_leaves_a_live_count_alone() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[answered_thread("a")]);
        app.diff_overlay = Some(overlay_for_answered_threads());
        hydrate_threads(&mut app);
        assert_eq!(waiting_count(&app), Some(1));

        let mut elsewhere = overlay_for_answered_threads();
        elsewhere.branch = Some("main".to_owned());
        app.diff_overlay = Some(elsewhere);
        hydrate_threads(&mut app);

        assert_eq!(waiting_count(&app), Some(1), "feat's answers still await a look");
    }

    #[test]
    fn replying_to_a_worker_answer_clears_the_waiting_signal() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[answered_thread("a")]);
        app.diff_overlay = Some(overlay_for_answered_threads());
        hydrate_threads(&mut app);
        assert_eq!(waiting_count(&app), Some(1), "lit before the reviewer answers");

        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = app.diff_overlay.as_ref().expect("overlay").comments[0].clone();
        let mut editor = InputState::new();
        editor.insert_str("still not right");
        app.diff_overlay.as_mut().expect("overlay").active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: Some(prior), edit_turn: None });
        save_active_input(&mut app);

        assert_eq!(waiting_count(&app), None, "the reviewer's own turn clears the signal");
    }

    #[test]
    fn resolving_a_worker_answer_clears_the_waiting_signal() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[answered_thread("a"), answered_thread("b")]);
        app.diff_overlay = Some(overlay_for_answered_threads());
        hydrate_threads(&mut app);
        assert_eq!(waiting_count(&app), Some(2), "both answers await a look");

        let resolved_key = app.diff_overlay.as_ref().expect("overlay").comments[0].key;
        apply_thread_action(
            &mut app,
            CommentRef { line: resolved_key, slot: 0 },
            ThreadAction::Resolve,
        );

        assert_eq!(waiting_count(&app), Some(1), "resolve is how a read answer is dismissed");
    }

    #[test]
    fn comment_button_click_resolves_only_the_clicked_thread() {
        let (mut app, _dir) = review_app();
        let files =
            vec![single_hunk_file("src/x.rs", vec![added_line("a", 10), added_line("b", 11)])];
        let overlay = DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        app.diff_overlay = Some(overlay);
        app.diff_overlay.as_mut().expect("overlay").branch = Some("feat".to_owned());
        let ka = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let kb = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        with_editor(app.diff_overlay.as_mut().expect("overlay"), ka, "thread A");
        save_active_input(&mut app);
        with_editor(app.diff_overlay.as_mut().expect("overlay"), kb, "thread B");
        save_active_input(&mut app);

        // Click B's Resolve button: it targets B by key, leaving A
        // untouched.
        apply_thread_action(&mut app, CommentRef { line: kb, slot: 0 }, ThreadAction::Resolve);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let status_of =
            |key: LineKey| overlay.comments.iter().find(|c| c.key == key).map(|c| c.thread.status);
        assert_eq!(status_of(kb), Some(ReviewStatus::Resolved), "the clicked thread resolves");
        assert_eq!(status_of(ka), Some(ReviewStatus::Open), "the other thread is untouched");

        let ws = app.workspace.clone().expect("ws");
        let threads = ws.load_review_threads("forge", "feat").expect("load");
        let persisted =
            |line: u32| threads.iter().find(|t| t.anchor.line == line).map(|t| t.status);
        assert_eq!(persisted(11), Some(ReviewStatus::Resolved), "B persisted resolved");
        assert_eq!(persisted(10), Some(ReviewStatus::Open), "A stays open in redb");
    }

    #[test]
    fn reopen_is_noop_on_an_open_thread() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("z", 3)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        // Reopen only moves a Resolved thread; an Open one is left alone.
        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Reopen);
        assert_eq!(thread_status(&app), ReviewStatus::Open, "reopen does not touch an open thread");
    }

    #[test]
    fn resolve_is_noop_when_key_has_no_thread() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("z", 3)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        // No comment at the key: the button action must not panic or write.
        apply_thread_action(
            &mut app,
            CommentRef { line: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, slot: 0 },
            ThreadAction::Resolve,
        );
        let ws = app.workspace.clone().expect("ws");
        assert!(ws.load_review_threads("forge", "feat").expect("load").is_empty());
    }

    #[test]
    fn comment_button_resolves_current_scope_thread_on_key_collision() {
        // On a single-commit branch the whole-diff and commit diffs share
        // a file layout, so a whole-diff comment and a commit-scoped one -
        // both durable now - can land on the same key. The button must act
        // on the current scope's thread, not whichever `.find` hits first.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.scope = DiffScope::WholeDiff;
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        // Commit-scoped comment pushed FIRST, with its own durable thread.
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 10,
            comment_text: "commit note".to_owned(),
            commit: Some("aaa".to_owned()),
            thread: forge_primitives::ReviewThread {
                id: "tc".to_owned(),
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
                    text: "commit note".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: String::new(),
                updated_at: String::new(),
                commit: Some("aaa".to_owned()),
            },
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        // Durable whole-diff thread at the SAME key.
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 10,
            comment_text: "durable".to_owned(),
            commit: None,
            thread: forge_primitives::ReviewThread {
                id: "t1".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 10,
                    content_hash: 0,
                    context: Vec::new(),
                    base_ref: "feat".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "durable".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: String::new(),
                updated_at: String::new(),
                commit: None,
            },
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        // In whole-diff scope the button targets the commit==None thread.
        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let durable = comments.iter().find(|c| c.commit.is_none()).expect("whole-diff comment");
        assert_eq!(
            durable.thread.status,
            ReviewStatus::Resolved,
            "the current scope's thread resolved despite the key collision",
        );
        let commit_scoped = comments.iter().find(|c| c.commit.is_some()).expect("commit comment");
        assert_eq!(
            commit_scoped.thread.status,
            ReviewStatus::Open,
            "the other scope's thread is untouched",
        );
    }

    #[test]
    fn save_then_hydrate_round_trips_in_place() {
        // Save-side capture (hash / side / context) must round-trip: an
        // unchanged file re-anchors the saved thread InPlace, Open, and
        // as a hydrated (not-session-authored) comment.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "bound check");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        // Reopen: re-hydrate against the same (unchanged) files.
        hydrate_threads(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(comment.key, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "InPlace");
        assert_eq!(comment.thread.status, ReviewStatus::Open);
        assert!(
            comment.authored_this_session,
            "a rebuild over a comment written this session keeps it session work",
        );
        assert!(comment.persisted, "hydrated comment is durable");
    }

    #[test]
    fn hydrate_scopes_to_target_and_preserves_other_target_threads() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, base_ref: &str, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 5,
                content_hash: resolver::anchor_hash(text),
                context: vec!["fn wrapper() {".to_owned(), "}".to_owned()],
                base_ref: base_ref.to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: None,
        };
        // Same branch, two whole-diff targets plus a commit-scoped thread.
        let mut c = seed("c", "main", "let c = 3;");
        c.commit = Some("deadbeef".to_owned());
        ws.save_review_threads(
            "forge",
            "feat",
            &[seed("a", "main", "let a = 1;"), seed("b", "HEAD", "let b = 2;"), c],
        );

        // Open against "main"; its thread drifts (line 5 -> 8), forcing a
        // writeback. The "HEAD"-target thread must survive that writeback.
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("fn wrapper() {", 7), added_line("let a = 1;", 8), added_line("}", 9)],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        // The union spans commits but not diff bases.
        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let mut ids: Vec<&str> = comments.iter().map(|c| c.thread.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["a", "c"], "both main-target threads render, whatever their commit");
        assert!(
            !ids.contains(&"b"),
            "a thread numbered against another base would land on unrelated code",
        );

        let reloaded = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(reloaded.len(), 3, "the other-target and commit-scoped threads survived");
        assert_eq!(
            reloaded.iter().find(|t| t.id == "a").expect("a").anchor.line,
            8,
            "the main-target thread re-anchored to the moved line",
        );
        assert!(reloaded.iter().any(|t| t.id == "b"), "the HEAD-target thread is preserved");
        assert!(
            reloaded.iter().any(|t| t.id == "c"),
            "the commit-scoped thread is preserved despite sharing the target base_ref",
        );
    }

    #[test]
    fn hydrate_shows_commit_scoped_thread_on_its_commit() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "c0".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: resolver::content_hash("let a = 1;"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "on commit zero".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: Some("sha0".to_owned()),
            }],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first")];
        overlay.scope = DiffScope::Commit(0);
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        assert_eq!(comments.len(), 1, "the commit-scoped thread hydrated onto its commit");
        let c = &comments[0];
        assert_eq!(c.thread.id, "c0");
        assert_eq!(c.commit.as_deref(), Some("sha0"), "rebuilt comment carries the scope sha");
        assert!(c.persisted, "hydrated comment is durable");
        assert!(!c.authored_this_session, "hydrated, not authored this session");
    }

    #[test]
    fn hydrate_isolates_by_commit_scope() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, sha: &str, line: u32, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line,
                content_hash: resolver::anchor_hash(text),
                context: vec!["fn wrapper() {".to_owned(), "}".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: Some(sha.to_owned()),
        };
        // The sha0 thread drifts (line 5 -> 8) so a writeback fires; the
        // sha1 thread must survive that writeback untouched.
        ws.save_review_threads(
            "forge",
            "feat",
            &[seed("c0", "sha0", 5, "let a = 1;"), seed("c1", "sha1", 5, "let b = 2;")],
        );
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("fn wrapper() {", 7), added_line("let a = 1;", 8), added_line("}", 9)],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        overlay.scope = DiffScope::Commit(0);
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        assert_eq!(comments.len(), 1, "only the current commit's thread renders");
        assert_eq!(comments[0].thread.id, "c0");

        let reloaded = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(reloaded.len(), 2, "the other commit's thread survives the writeback");
        assert_eq!(
            reloaded.iter().find(|t| t.id == "c0").expect("c0").anchor.line,
            8,
            "the current commit's thread re-anchored to the moved line",
        );
        assert!(reloaded.iter().any(|t| t.id == "c1"), "the sha1 thread is preserved");
    }

    #[test]
    fn hydrate_isolates_by_commit_scope_reverse() {
        // The mirror of the above from the other side: in Commit(1) scope
        // only the sha1 thread renders; the sha0 thread stays out.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, sha: &str, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 5,
                content_hash: resolver::content_hash(text),
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: Some(sha.to_owned()),
        };
        ws.save_review_threads(
            "forge",
            "feat",
            &[seed("c0", "sha0", "let b = 2;"), seed("c1", "sha1", "let a = 1;")],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        overlay.scope = DiffScope::Commit(1);
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        assert_eq!(comments.len(), 1, "only the sha1 thread renders in Commit(1)");
        assert_eq!(comments[0].thread.id, "c1");
        assert!(
            comments.iter().all(|c| c.thread.id != "c0"),
            "the sha0 thread stays out of the Commit(1) scope",
        );
        assert!(
            ws.load_review_threads("forge", "feat").expect("load").iter().any(|t| t.id == "c0"),
            "the sha0 thread is preserved in the store",
        );
    }

    #[test]
    fn commit_scoped_thread_hydrates_back_resolved() {
        // End-to-end state survival: a Resolved commit-scoped thread
        // reopened on its commit hydrates back Resolved.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "c0".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: resolver::content_hash("let a = 1;"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "resolved earlier".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Resolved,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: Some("sha0".to_owned()),
            }],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first")];
        overlay.scope = DiffScope::Commit(0);
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(
            comment.thread.status,
            ReviewStatus::Resolved,
            "the commit-scoped thread hydrated back Resolved",
        );
    }

    #[test]
    fn hydrate_surfaces_a_review_load_error() {
        // A corrupt persisted row makes the load fail; hydrate must set the
        // visible-notice state rather than leave a silently-empty pane.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_row_for_test(&db, "forge", "feat")
            .expect("write corrupt row");
        workspace.install_db_for_test(db);
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);

        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        assert!(
            app.diff_overlay.as_ref().expect("overlay").review_load_error.is_some(),
            "a load failure surfaces the review-load notice state, not a blank pane",
        );
    }

    #[test]
    fn hydrate_surfaces_a_corrupt_reviews_row() {
        // The `reviews` table is a separate row from `review_threads`; a
        // corrupt reviews blob must surface the same banner, not silently
        // degrade every chip to `· unfiled`.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_reviews_row_for_test(&db, "forge", "feat")
            .expect("write corrupt reviews row");
        workspace.install_db_for_test(db);
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);

        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        assert!(
            app.diff_overlay.as_ref().expect("overlay").review_load_error.is_some(),
            "a corrupt reviews row surfaces the load notice, not a silent unfiled degrade",
        );
    }

    #[test]
    fn hydrate_populates_reviews_from_the_store() {
        // The `· R#` chip tag + the `l` list both read `overlay.reviews`,
        // which hydrate fills from the store - pin that it actually lands.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.submit_review(
            "forge",
            "feat",
            Some("first pass".to_owned()),
            &[],
            forge_workspace::SessionKey::from_session_id("reviewer"),
        )
        .expect("seal a review");

        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let reviews = &app.diff_overlay.as_ref().expect("overlay").reviews;
        assert_eq!(reviews.len(), 1, "hydrate loaded the submitted review");
        assert_eq!(reviews[0].number, 1);
        assert_eq!(reviews[0].summary.as_deref(), Some("first pass"));
    }

    #[test]
    fn hydrate_whole_diff_takes_commit_scoped_threads_too() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, commit: Option<&str>, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 5,
                content_hash: resolver::content_hash(text),
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: commit.map(str::to_owned),
        };
        // Neither seed records context, and the hunk is one line, so
        // neither re-anchors: both land through the outdated fallback and
        // stack on the only line there is. The writeback is the resulting
        // Open-to-Outdated flip, not a change of line.
        ws.save_review_threads(
            "forge",
            "feat",
            &[seed("wd", None, "let a = 1;"), seed("cs", Some("sha0"), "let a = 1;")],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 8)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let ids: Vec<&str> = comments.iter().map(|c| c.thread.id.as_str()).collect();
        assert_eq!(ids, vec!["wd", "cs"], "the whole diff renders both, commit-scoped included");
        assert!(
            comments.iter().all(|c| c.commit.is_none()),
            "a rendered comment carries the scope it is drawn in, not the one it was authored in",
        );
        assert_eq!(
            comments[0].key, comments[1].key,
            "same line, so they stack on one key and each needs its own click target",
        );

        let reloaded = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(reloaded.len(), 2, "both threads survive");
        let cs = reloaded.iter().find(|t| t.id == "cs").expect("the commit-scoped thread");
        assert_eq!(
            cs.commit.as_deref(),
            Some("sha0"),
            "rendering it in the union does not rewrite which commit it was authored against",
        );
    }

    #[test]
    fn hydrate_replaces_only_the_current_scope_and_keeps_others() {
        // `retain(|c| c.commit != scope_commit)` must drop ONLY the current
        // scope's in-memory comments (replaced by the rebuilt set) and keep
        // other scopes' comments. An inverted retain or a blanket clear
        // would strand the other-scope comment.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "wd".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: resolver::content_hash("let a = 1;"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "hydrated".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: None,
            }],
        );

        let mut overlay = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])],
        );
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        // An OTHER-scope (commit sha1) comment that must survive, and a stale
        // current-scope (whole-diff) comment the hydrate replaces.
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 9,
            comment_text: "on sha1".to_owned(),
            commit: Some("sha1".to_owned()),
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "stale whole-diff".to_owned(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            // Written to the store, and no longer in it: superseded.
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let other = comments
            .iter()
            .find(|c| c.commit.as_deref() == Some("sha1"))
            .expect("the other-scope comment survives");
        assert_eq!(other.comment_text, "on sha1");
        let whole: Vec<_> = comments.iter().filter(|c| c.commit.is_none()).collect();
        assert_eq!(whole.len(), 1, "one whole-diff comment after hydrate");
        assert_eq!(
            whole[0].thread.id, "wd",
            "the stale in-memory whole-diff comment was replaced by the hydrated thread",
        );
    }

    #[test]
    fn resolve_flips_an_outdated_thread_to_resolved() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        // Simulate the thread having drifted to Outdated.
        if let Some(o) = app.diff_overlay.as_mut() {
            o.comments[0].thread.status = ReviewStatus::Outdated;
        }
        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);
        assert_eq!(thread_status(&app), ReviewStatus::Resolved, "outdated resolves to resolved");
    }
}
