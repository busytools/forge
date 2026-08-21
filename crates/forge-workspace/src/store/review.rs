//! Review-thread persistence on the redb `review_threads` table.
//!
//! The `/diff` overlay's line comments persist here per `(project,
//! branch)` so a review round survives closing the overlay and forge
//! restarts. Keyed by `(project, branch)` - the whole set for a branch
//! is one serde-json blob. `project` is the forge.toml project NAME
//! (worktree-agnostic), never the directory-hash key.

use anyhow::Context;
use forge_primitives::review::{
    ReviewAuthor, ReviewComment, ReviewSet, ReviewStatus, ReviewThread,
};
use redb::{ReadableTable, TableDefinition};

use super::Db;

const REVIEW_THREADS: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("review_threads");
/// Submitted reviews per `(project, branch)` - the sealed groupings a
/// thread's turns point into. Sibling of [`REVIEW_THREADS`]; the whole set
/// for a branch is one serde-json blob.
const REVIEWS: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("reviews");

/// Load every thread for `(project, branch)`. Empty when the branch has
/// no row (or the table doesn't exist yet).
pub fn load(db: &Db, project: &str, branch: &str) -> anyhow::Result<Vec<ReviewThread>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(REVIEW_THREADS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    match table.get((project, branch))? {
        Some(value) => decode(value.value(), project, branch),
        None => Ok(Vec::new()),
    }
}

/// Overwrite the whole thread set for `(project, branch)`. An empty
/// slice removes the row rather than storing an empty blob.
pub fn save(db: &Db, project: &str, branch: &str, threads: &[ReviewThread]) -> anyhow::Result<()> {
    if threads.is_empty() {
        delete(db, project, branch)?;
        return Ok(());
    }
    let value = serde_json::to_vec(threads).context("serialize review threads")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(REVIEW_THREADS)?;
        table.insert((project, branch), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Insert `thread`, replacing any existing thread with the same id in
/// the branch's set; appends when the id is new. Stamps `updated_at` to
/// now and fills `created_at` (preserved from the existing row on
/// replace, set to now when the caller left it empty) so the TUI need
/// not carry a clock.
pub fn upsert(
    db: &Db,
    project: &str,
    branch: &str,
    mut thread: ReviewThread,
) -> anyhow::Result<()> {
    let now = rfc3339_now();
    thread.updated_at.clone_from(&now);
    // Stamp any comment the caller left unstamped (the TUI carries no
    // clock), so a persisted comment's `at` is a real rfc3339 time.
    for comment in &mut thread.comments {
        if comment.at.is_empty() {
            comment.at.clone_from(&now);
        }
    }
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(REVIEW_THREADS)?;
        let mut threads = match table.get((project, branch))? {
            Some(value) => decode(value.value(), project, branch)?,
            None => Vec::new(),
        };
        if let Some(existing) = threads.iter_mut().find(|t| t.id == thread.id) {
            thread.created_at.clone_from(&existing.created_at);
            *existing = thread;
        } else {
            if thread.created_at.is_empty() {
                thread.created_at = now;
            }
            threads.push(thread);
        }
        let value = serde_json::to_vec(&threads).context("serialize review threads")?;
        table.insert((project, branch), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Remove the single thread `id` from `(project, branch)`, dropping the
/// row entirely when it was the last one. Returns whether a thread was
/// removed. Used when the user clears a reopened comment's text to
/// delete it, so it doesn't resurrect on the next hydrate.
pub fn remove_thread(db: &Db, project: &str, branch: &str, id: &str) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let removed = {
        let mut table = txn.open_table(REVIEW_THREADS)?;
        let mut threads = match table.get((project, branch))? {
            Some(value) => decode(value.value(), project, branch)?,
            None => Vec::new(),
        };
        let before = threads.len();
        threads.retain(|t| t.id != id);
        let removed = threads.len() != before;
        if removed {
            if threads.is_empty() {
                table.remove((project, branch))?;
            } else {
                let value = serde_json::to_vec(&threads).context("serialize review threads")?;
                table.insert((project, branch), value.as_slice())?;
            }
        }
        removed
    };
    txn.commit()?;
    Ok(removed)
}

/// Set the status of the thread `id` in `(project, branch)`, bumping its
/// `updated_at`. Returns whether a matching thread was found.
pub fn set_status(
    db: &Db,
    project: &str,
    branch: &str,
    id: &str,
    status: ReviewStatus,
) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let found = {
        let mut table = txn.open_table(REVIEW_THREADS)?;
        let mut threads = match table.get((project, branch))? {
            Some(value) => decode(value.value(), project, branch)?,
            None => Vec::new(),
        };
        match threads.iter_mut().find(|t| t.id == id) {
            Some(thread) => {
                thread.status = status;
                thread.updated_at = rfc3339_now();
                let encoded = serde_json::to_vec(&threads).context("serialize review threads")?;
                table.insert((project, branch), encoded.as_slice())?;
                true
            }
            None => false,
        }
    };
    txn.commit()?;
    Ok(found)
}

/// Look up one thread by its globally-unique `id` in `(project, branch)`.
/// `None` when the branch has no such thread (or no row). `comment_id` in
/// the review MCP maps to this `id`.
pub fn find_thread_by_id(
    db: &Db,
    project: &str,
    branch: &str,
    id: &str,
) -> anyhow::Result<Option<ReviewThread>> {
    Ok(load(db, project, branch)?.into_iter().find(|t| t.id == id))
}

/// Append an agent reply to the thread `id` in `(project, branch)`: push a
/// `ReviewComment` authored by `Agent { label: author_label }`, flip an
/// `Open` thread to `Addressed` (a Resolved / Outdated / already-Addressed
/// thread keeps its state), bump `updated_at`, and persist in one write
/// transaction. Returns the thread's status after the append. Errors when
/// no thread carries `id`, so a stale comment_id surfaces rather than
/// silently no-op'ing. An empty `at` is stamped with the current instant.
pub fn append_reply(
    db: &Db,
    project: &str,
    branch: &str,
    id: &str,
    author_label: &str,
    text: &str,
    at: &str,
) -> anyhow::Result<ReviewStatus> {
    let stamp = if at.is_empty() { rfc3339_now() } else { at.to_owned() };
    let txn = db.database().begin_write()?;
    let status = {
        let mut table = txn.open_table(REVIEW_THREADS)?;
        let mut threads = match table.get((project, branch))? {
            Some(value) => decode(value.value(), project, branch)?,
            None => Vec::new(),
        };
        let thread = threads
            .iter_mut()
            .find(|t| t.id == id)
            .with_context(|| format!("no review thread {id} in ({project}, {branch})"))?;
        thread.comments.push(ReviewComment {
            author: ReviewAuthor::Agent { label: author_label.to_owned() },
            text: text.to_owned(),
            at: stamp.clone(),
            review_id: None,
        });
        if thread.status == ReviewStatus::Open {
            thread.status = ReviewStatus::Addressed;
        }
        thread.updated_at = stamp;
        let status = thread.status;
        let encoded = serde_json::to_vec(&threads).context("serialize review threads")?;
        table.insert((project, branch), encoded.as_slice())?;
        status
    };
    txn.commit()?;
    Ok(status)
}

/// Delete the whole thread set for `(project, branch)`. Returns whether
/// a row existed.
pub fn delete(db: &Db, project: &str, branch: &str) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let existed = {
        let mut table = txn.open_table(REVIEW_THREADS)?;
        table.remove((project, branch))?.is_some()
    };
    txn.commit()?;
    Ok(existed)
}

/// Load every submitted review for `(project, branch)`, oldest first.
/// Empty when the branch has no row (or the table doesn't exist yet).
pub fn load_reviews(db: &Db, project: &str, branch: &str) -> anyhow::Result<Vec<ReviewSet>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(REVIEWS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    match table.get((project, branch))? {
        Some(value) => decode_reviews(value.value(), project, branch),
        None => Ok(Vec::new()),
    }
}

/// The branches under `project` that hold at least one submitted review,
/// in key order. Seeks straight to the project's first key and stops at
/// its last, so the `review__list` empty path can ask "is this review
/// filed against another branch?" without scanning the table.
pub fn review_branches(db: &Db, project: &str) -> anyhow::Result<Vec<String>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(REVIEWS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut branches = Vec::new();
    for row in table.range((project, "")..)? {
        let (key, _) = row?;
        let (row_project, branch) = key.value();
        if row_project != project {
            break;
        }
        branches.push(branch.to_owned());
    }
    Ok(branches)
}

/// Drop both of `(project, branch)`'s rows in one write transaction.
///
/// One transaction because the two are halves of one fact. Deleting them
/// separately leaves a window where a crash strands a reviews row whose
/// threads are gone - `review__get` would then resolve a review whose
/// comments no longer exist.
pub fn delete_branch_state(db: &Db, project: &str, branch: &str) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        txn.open_table(REVIEW_THREADS)?.remove((project, branch))?;
        txn.open_table(REVIEWS)?.remove((project, branch))?;
    }
    txn.commit()?;
    Ok(())
}

/// Overwrite the whole review set for `(project, branch)`. An empty slice
/// removes the row rather than storing an empty blob.
pub fn save_reviews(
    db: &Db,
    project: &str,
    branch: &str,
    reviews: &[ReviewSet],
) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(REVIEWS)?;
        if reviews.is_empty() {
            table.remove((project, branch))?;
        } else {
            let value = serde_json::to_vec(reviews).context("serialize reviews")?;
            table.insert((project, branch), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// Delete the whole review set for `(project, branch)` on branch/worktree
/// teardown, so a reused branch doesn't inherit phantom reviews. Routes
/// through [`save_reviews`] with an empty slice (which drops the row).
pub fn delete_reviews(db: &Db, project: &str, branch: &str) -> anyhow::Result<()> {
    save_reviews(db, project, branch, &[])
}

/// Seal a new review for `(project, branch)`: mint its number (existing
/// count + 1), stamp every still-unfiled user turn on each listed thread
/// with the new review id, and append the [`ReviewSet`]. Turns already
/// filed never move, so a thread whose earlier turns went into review 1
/// and whose reply goes into review 2 belongs to both. A thread that
/// gains a turn this way is `Addressed` no longer - the agent owes an
/// answer again - so it flips back to `Open`; a `Resolved` / `Outdated`
/// thread keeps its state. Agent replies are answers to a round rather
/// than part of one and are never stamped. One write transaction spans
/// both tables so the stamp and the append land together.
pub fn submit_review(
    db: &Db,
    project: &str,
    branch: &str,
    summary: Option<String>,
    thread_ids: &[String],
) -> anyhow::Result<ReviewSet> {
    let now = rfc3339_now();
    let txn = db.database().begin_write()?;
    let review = {
        let mut reviews_table = txn.open_table(REVIEWS)?;
        let mut reviews = match reviews_table.get((project, branch))? {
            Some(value) => decode_reviews(value.value(), project, branch)?,
            None => Vec::new(),
        };
        let number = u32::try_from(reviews.len()).unwrap_or(u32::MAX).saturating_add(1);
        let review =
            ReviewSet { id: uuid::Uuid::new_v4().to_string(), number, summary, created_at: now };
        {
            let mut threads_table = txn.open_table(REVIEW_THREADS)?;
            // Decode into an owned value so the read guard's borrow ends
            // before the insert below.
            let existing = match threads_table.get((project, branch))? {
                Some(value) => Some(decode(value.value(), project, branch)?),
                None => None,
            };
            if let Some(mut threads) = existing {
                let mut changed = false;
                for thread in threads.iter_mut().filter(|t| thread_ids.contains(&t.id)) {
                    let mut sealed = false;
                    for turn in thread
                        .comments
                        .iter_mut()
                        .filter(|c| matches!(c.author, ReviewAuthor::User) && c.review_id.is_none())
                    {
                        turn.review_id = Some(review.id.clone());
                        sealed = true;
                    }
                    if sealed && thread.status == ReviewStatus::Addressed {
                        thread.status = ReviewStatus::Open;
                    }
                    changed |= sealed;
                }
                if changed {
                    let encoded =
                        serde_json::to_vec(&threads).context("serialize review threads")?;
                    threads_table.insert((project, branch), encoded.as_slice())?;
                }
            }
        }
        reviews.push(review.clone());
        let encoded = serde_json::to_vec(&reviews).context("serialize reviews")?;
        reviews_table.insert((project, branch), encoded.as_slice())?;
        review
    };
    txn.commit()?;
    Ok(review)
}

/// Decode a stored blob into threads, mapping a decode failure to an
/// error tagged with the owning `(project, branch)` so a corrupt row is
/// diagnosable rather than a bare serde message. Lifts membership off any
/// row still carrying it on the thread (see [`lift_thread_membership`]).
fn decode(bytes: &[u8], project: &str, branch: &str) -> anyhow::Result<Vec<ReviewThread>> {
    let mut threads: Vec<ReviewThread> = serde_json::from_slice(bytes)
        .with_context(|| format!("decode review threads for ({project}, {branch})"))?;
    lift_thread_membership(bytes, &mut threads);
    Ok(threads)
}

/// Membership a row carries on the thread rather than its turns - the
/// shape stored while a thread could only belong to one review.
#[derive(serde::Deserialize)]
struct ThreadMembership {
    #[serde(default)]
    review_id: Option<String>,
}

/// Attribute a pre-per-turn row's history: a thread filed as a whole has
/// its user turns stamped with that review, so they read as the round they
/// were actually part of instead of as unfiled. Positional - the mirror
/// decode ignores every other key, so it aligns with `threads`. Only
/// applies where no turn is filed yet, leaving a re-saved row's own
/// membership alone; the new shape persists on the thread's next write.
fn lift_thread_membership(bytes: &[u8], threads: &mut [ReviewThread]) {
    let Ok(stored) = serde_json::from_slice::<Vec<ThreadMembership>>(bytes) else {
        return;
    };
    for (thread, legacy) in threads.iter_mut().zip(stored) {
        let Some(review_id) = legacy.review_id else { continue };
        if thread.comments.iter().any(|c| c.review_id.is_some()) {
            continue;
        }
        for turn in thread.comments.iter_mut().filter(|c| matches!(c.author, ReviewAuthor::User)) {
            turn.review_id = Some(review_id.clone());
        }
    }
}

/// Decode a stored blob into reviews, tagged with the owning `(project,
/// branch)` on failure so a corrupt row is diagnosable.
fn decode_reviews(bytes: &[u8], project: &str, branch: &str) -> anyhow::Result<Vec<ReviewSet>> {
    serde_json::from_slice(bytes)
        .with_context(|| format!("decode reviews for ({project}, {branch})"))
}

/// RFC3339 timestamp for the current instant, matching the `mcp::peers`
/// helper's shape.
fn rfc3339_now() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "rfc3339 format failed; emitting epoch sentinel");
        "1970-01-01T00:00:00Z".to_owned()
    })
}

/// Test-only: write undecodable bytes into the `(project, branch)` row
/// so a caller can exercise the load-error path (a corrupt / partially
/// written redb row). Feature-gated for cross-crate test access.
#[cfg(feature = "test-helpers")]
pub fn write_corrupt_row_for_test(db: &Db, project: &str, branch: &str) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(REVIEW_THREADS)?;
        table.insert((project, branch), b"not json".as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Test-only sibling of [`write_corrupt_row_for_test`] for the `reviews`
/// table, so a caller can exercise the reviews load-error path.
#[cfg(feature = "test-helpers")]
pub fn write_corrupt_reviews_row_for_test(
    db: &Db,
    project: &str,
    branch: &str,
) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(REVIEWS)?;
        table.insert((project, branch), b"not json".as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::review::{
        ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSet, ReviewSide,
    };
    use tempfile::tempdir;

    fn thread(id: &str, line: u32) -> ReviewThread {
        ReviewThread {
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
                text: format!("comment {id}"),
                at: "2026-07-19T10:00:00Z".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-19T10:00:00Z".to_owned(),
            updated_at: "2026-07-19T10:00:00Z".to_owned(),
            commit: None,
        }
    }

    /// Append an unfiled user turn, as the overlay does when the reviewer
    /// replies on a thread.
    fn reply(db: &Db, id: &str, text: &str) {
        let mut thread =
            find_thread_by_id(db, "forge", "feat", id).expect("load").expect("thread for reply");
        thread.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: text.to_owned(),
            at: String::new(),
            review_id: None,
        });
        upsert(db, "forge", "feat", thread).expect("upsert reply");
    }

    fn open_db() -> (tempfile::TempDir, Db) {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        (dir, db)
    }

    #[test]
    fn save_then_load_round_trips() {
        let (_dir, db) = open_db();
        let threads = vec![thread("a", 10), thread("b", 20)];
        save(&db, "forge", "feat", &threads).expect("save");
        assert_eq!(load(&db, "forge", "feat").expect("load"), threads);
    }

    #[test]
    fn load_is_empty_on_miss() {
        let (_dir, db) = open_db();
        assert!(load(&db, "forge", "nope").expect("load").is_empty());
    }

    #[test]
    fn upsert_replaces_by_id_and_appends_new() {
        let (_dir, db) = open_db();
        upsert(&db, "forge", "feat", thread("a", 10)).expect("upsert a");
        upsert(&db, "forge", "feat", thread("b", 20)).expect("upsert b");
        // Re-upsert of "a" with a changed anchor replaces, never duplicates.
        let mut a2 = thread("a", 10);
        a2.anchor.line = 99;
        upsert(&db, "forge", "feat", a2).expect("re-upsert a");
        let loaded = load(&db, "forge", "feat").expect("load");
        assert_eq!(loaded.len(), 2, "re-upsert replaced, no duplicate");
        assert_eq!(loaded.iter().find(|t| t.id == "a").expect("a present").anchor.line, 99);
    }

    #[test]
    fn set_status_mutates_one_and_reports_missing() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10), thread("b", 20)]).expect("save");
        assert!(set_status(&db, "forge", "feat", "a", ReviewStatus::Resolved).expect("set"));
        let loaded = load(&db, "forge", "feat").expect("load");
        assert_eq!(loaded.iter().find(|t| t.id == "a").expect("a").status, ReviewStatus::Resolved);
        assert_eq!(loaded.iter().find(|t| t.id == "b").expect("b").status, ReviewStatus::Open);
        assert!(
            !set_status(&db, "forge", "feat", "missing", ReviewStatus::Resolved).expect("set"),
            "an unknown id reports not-found",
        );
    }

    #[test]
    fn upsert_stamps_an_empty_comment_at() {
        let (_dir, db) = open_db();
        let mut t = thread("a", 10);
        t.comments[0].at = String::new();
        upsert(&db, "forge", "feat", t).expect("upsert");
        let loaded = load(&db, "forge", "feat").expect("load");
        assert!(!loaded[0].comments[0].at.is_empty(), "upsert stamps an empty comment `at`");
    }

    #[test]
    fn remove_thread_drops_one_and_reports_missing() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10), thread("b", 20)]).expect("save");
        assert!(remove_thread(&db, "forge", "feat", "a").expect("remove a"));
        let loaded = load(&db, "forge", "feat").expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "b");
        assert!(!remove_thread(&db, "forge", "feat", "a").expect("remove again"), "already gone");
        // Removing the last thread clears the row.
        assert!(remove_thread(&db, "forge", "feat", "b").expect("remove b"));
        assert!(load(&db, "forge", "feat").expect("load").is_empty());
    }

    #[test]
    fn delete_removes_the_branch_set() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save");
        assert!(delete(&db, "forge", "feat").expect("delete"), "existing row deletes to true");
        assert!(load(&db, "forge", "feat").expect("load").is_empty());
        assert!(!delete(&db, "forge", "feat").expect("delete"), "absent row deletes to false");
    }

    #[test]
    fn distinct_project_branch_keys_do_not_collide() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save feat");
        save(&db, "forge", "other", &[thread("b", 20), thread("c", 30)]).expect("save other");
        save(&db, "elsewhere", "feat", &[thread("d", 40)]).expect("save elsewhere");
        assert_eq!(load(&db, "forge", "feat").expect("load").len(), 1);
        assert_eq!(load(&db, "forge", "other").expect("load").len(), 2);
        assert_eq!(load(&db, "elsewhere", "feat").expect("load").len(), 1);
        // Deleting one key leaves the others untouched.
        delete(&db, "forge", "feat").expect("delete");
        assert!(load(&db, "forge", "feat").expect("load").is_empty());
        assert_eq!(load(&db, "forge", "other").expect("load").len(), 2);
        assert_eq!(load(&db, "elsewhere", "feat").expect("load").len(), 1);
    }

    #[test]
    fn corrupt_row_surfaces_error_and_is_never_clobbered() {
        let (_dir, db) = open_db();
        // Undecodable bytes directly in the row (a corrupt / partial write).
        {
            let txn = db.database().begin_write().expect("begin");
            {
                let mut table = txn.open_table(REVIEW_THREADS).expect("open");
                table.insert(("forge", "feat"), b"not json".as_slice()).expect("insert");
            }
            txn.commit().expect("commit");
        }
        // `load` surfaces the decode failure rather than a silent empty vec.
        assert!(load(&db, "forge", "feat").is_err(), "a corrupt row is an error, not empty");
        // Every write path re-decodes with `?` and aborts before writing, so
        // the corrupt bytes survive intact (the non-clobber guarantee).
        assert!(upsert(&db, "forge", "feat", thread("a", 1)).is_err(), "upsert aborts");
        assert!(
            set_status(&db, "forge", "feat", "a", ReviewStatus::Resolved).is_err(),
            "set_status aborts",
        );
        assert!(remove_thread(&db, "forge", "feat", "a").is_err(), "remove aborts");
        let txn = db.database().begin_read().expect("begin read");
        let table = txn.open_table(REVIEW_THREADS).expect("open");
        let raw = table.get(("forge", "feat")).expect("get").expect("row present");
        assert_eq!(raw.value(), b"not json".as_slice(), "the corrupt bytes were never overwritten");
    }

    #[test]
    fn save_empty_slice_clears_the_row() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save");
        save(&db, "forge", "feat", &[]).expect("save empty");
        assert!(load(&db, "forge", "feat").expect("load").is_empty());
    }

    #[test]
    fn append_reply_appends_agent_comment_and_flips_open_to_addressed() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save");
        let status = append_reply(
            &db,
            "forge",
            "feat",
            "a",
            "implementer",
            "fixed it",
            "2026-07-23T11:00:00Z",
        )
        .expect("append");
        assert_eq!(status, ReviewStatus::Addressed, "an Open thread flips to Addressed on reply");
        let loaded = load(&db, "forge", "feat").expect("load");
        let a = loaded.iter().find(|t| t.id == "a").expect("a present");
        assert_eq!(a.comments.len(), 2, "the agent reply appends to the thread");
        assert_eq!(a.comments[1].author, ReviewAuthor::Agent { label: "implementer".to_owned() });
        assert_eq!(a.comments[1].text, "fixed it");
        assert_eq!(a.comments[1].at, "2026-07-23T11:00:00Z");
        assert_eq!(a.status, ReviewStatus::Addressed);
    }

    #[test]
    fn append_reply_on_resolved_thread_appends_but_keeps_resolved() {
        let (_dir, db) = open_db();
        let mut t = thread("a", 10);
        t.status = ReviewStatus::Resolved;
        save(&db, "forge", "feat", &[t]).expect("save");
        let status =
            append_reply(&db, "forge", "feat", "a", "impl", "more", "2026-07-23T11:00:00Z")
                .expect("append");
        assert_eq!(status, ReviewStatus::Resolved, "a resolved thread stays resolved");
        let loaded = load(&db, "forge", "feat").expect("load");
        let a = loaded.iter().find(|t| t.id == "a").expect("a");
        assert_eq!(a.comments.len(), 2, "the reply still appends");
        assert_eq!(a.status, ReviewStatus::Resolved);
    }

    #[test]
    fn append_reply_unknown_id_errors() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save");
        assert!(
            append_reply(&db, "forge", "feat", "missing", "impl", "x", "2026-07-23T11:00:00Z")
                .is_err(),
            "an unknown comment_id is an error, not a silent no-op",
        );
    }

    #[test]
    fn find_thread_by_id_returns_the_thread_or_none() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10), thread("b", 20)]).expect("save");
        let found = find_thread_by_id(&db, "forge", "feat", "b").expect("lookup");
        assert_eq!(found.map(|t| t.id), Some("b".to_owned()));
        assert!(
            find_thread_by_id(&db, "forge", "feat", "missing").expect("lookup").is_none(),
            "an unknown id resolves to None",
        );
    }

    fn review(number: u32, summary: Option<&str>) -> ReviewSet {
        ReviewSet {
            id: format!("review-{number}"),
            number,
            summary: summary.map(str::to_owned),
            created_at: "2026-07-23T10:00:00Z".to_owned(),
        }
    }

    #[test]
    fn reviews_save_then_load_round_trips() {
        let (_dir, db) = open_db();
        let reviews = vec![review(1, Some("first")), review(2, None)];
        save_reviews(&db, "forge", "feat", &reviews).expect("save");
        assert_eq!(load_reviews(&db, "forge", "feat").expect("load"), reviews);
    }

    #[test]
    fn load_reviews_is_empty_on_miss() {
        let (_dir, db) = open_db();
        assert!(load_reviews(&db, "forge", "nope").expect("load").is_empty());
    }

    #[test]
    fn review_branches_lists_only_the_projects_own_branches() {
        let (_dir, db) = open_db();
        save_reviews(&db, "forge", "feat/a", &[review(1, None)]).expect("save a");
        save_reviews(&db, "forge", "feat/b", &[review(1, None)]).expect("save b");
        save_reviews(&db, "elsewhere", "feat/c", &[review(1, None)]).expect("save c");
        // A branch with threads but no submitted review is not a branch
        // with reviews.
        save(&db, "forge", "drafts-only", &[thread("a", 10)]).expect("save threads");
        assert_eq!(
            review_branches(&db, "forge").expect("branches"),
            vec!["feat/a".to_owned(), "feat/b".to_owned()],
        );
        assert_eq!(review_branches(&db, "nosuch").expect("branches"), Vec::<String>::new());
    }

    /// The range starts at the project's first key, so a project that
    /// sorts after another doesn't pick up its neighbour's rows.
    #[test]
    fn review_branches_does_not_bleed_across_adjacent_projects() {
        let (_dir, db) = open_db();
        save_reviews(&db, "aaa", "feat/x", &[review(1, None)]).expect("save aaa");
        save_reviews(&db, "aaa-suffix", "feat/y", &[review(1, None)]).expect("save suffix");
        save_reviews(&db, "bbb", "feat/z", &[review(1, None)]).expect("save bbb");
        assert_eq!(review_branches(&db, "aaa").expect("branches"), vec!["feat/x".to_owned()]);
        assert_eq!(
            review_branches(&db, "aaa-suffix").expect("branches"),
            vec!["feat/y".to_owned()]
        );
    }

    /// Both rows are halves of one fact. Deleting them separately
    /// leaves a window where a crash strands a reviews row whose threads
    /// are gone, and `review__get` would resolve a review whose comments
    /// no longer exist.
    #[test]
    fn delete_branch_state_clears_both_tables_and_spares_other_branches() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save threads");
        save_reviews(&db, "forge", "feat", &[review(1, None)]).expect("save reviews");
        save(&db, "forge", "other", &[thread("b", 20)]).expect("save other threads");
        save_reviews(&db, "forge", "other", &[review(1, None)]).expect("save other reviews");

        delete_branch_state(&db, "forge", "feat").expect("delete");

        assert!(load(&db, "forge", "feat").expect("load").is_empty(), "threads gone");
        assert!(load_reviews(&db, "forge", "feat").expect("load").is_empty(), "reviews gone too");
        assert_eq!(load(&db, "forge", "other").expect("load").len(), 1, "scoped to its branch");
        assert_eq!(load_reviews(&db, "forge", "other").expect("load").len(), 1);
        // A branch with no rows at all is not an error.
        delete_branch_state(&db, "forge", "absent").expect("absent branch deletes cleanly");
    }

    #[test]
    fn save_reviews_empty_slice_clears_the_row() {
        let (_dir, db) = open_db();
        save_reviews(&db, "forge", "feat", &[review(1, None)]).expect("save");
        save_reviews(&db, "forge", "feat", &[]).expect("save empty");
        assert!(load_reviews(&db, "forge", "feat").expect("load").is_empty());
    }

    #[test]
    fn submit_review_mints_number_and_files_listed_unfiled_threads() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10), thread("b", 20), thread("c", 30)])
            .expect("save threads");

        let filed = |id: &str| {
            load(&db, "forge", "feat")
                .expect("load")
                .into_iter()
                .find(|t| t.id == id)
                .expect("thread")
                .origin_review()
                .map(str::to_owned)
        };

        let r1 = submit_review(
            &db,
            "forge",
            "feat",
            Some("first pass".to_owned()),
            &["a".to_owned(), "b".to_owned()],
        )
        .expect("submit");
        assert_eq!(r1.number, 1);
        assert_eq!(r1.summary.as_deref(), Some("first pass"));
        assert_eq!(filed("a"), Some(r1.id.clone()), "a filed into r1");
        assert_eq!(filed("b"), Some(r1.id.clone()), "b filed into r1");
        assert_eq!(filed("c"), None, "c not listed, stays unfiled");
        assert_eq!(load_reviews(&db, "forge", "feat").expect("load").len(), 1);

        // A second submit mints number 2 and files only c; a/b keep r1.
        let r2 = submit_review(&db, "forge", "feat", None, &["c".to_owned()]).expect("submit 2");
        assert_eq!(r2.number, 2);
        assert_ne!(r2.id, r1.id, "each review has a distinct id");
        assert_eq!(filed("a"), Some(r1.id.clone()), "a stays in r1");
        assert_eq!(filed("c"), Some(r2.id.clone()), "c filed into r2");
        assert_eq!(load_reviews(&db, "forge", "feat").expect("load").len(), 2);
    }

    #[test]
    fn submit_review_leaves_an_already_filed_turn_untouched() {
        let (_dir, db) = open_db();
        let mut a = thread("a", 10);
        a.comments[0].review_id = Some("prior".to_owned());
        save(&db, "forge", "feat", &[a]).expect("save");
        let r = submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit");
        let stored = load(&db, "forge", "feat").expect("load");
        assert_eq!(
            stored[0].comments[0].review_id,
            Some("prior".to_owned()),
            "a sealed turn never moves to a later review",
        );
        assert!(!stored[0].is_in_review(&r.id), "with nothing new to seal it joins no review");
        assert_eq!(r.number, 1, "the review still mints even if it files nothing new");
    }

    #[test]
    fn a_reply_on_a_filed_thread_joins_the_next_review() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save");
        let r1 = submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit 1");
        append_reply(&db, "forge", "feat", "a", "implementer", "done", "").expect("agent reply");
        reply(&db, "a", "not quite");
        let r2 = submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit 2");

        let stored = load(&db, "forge", "feat").expect("load").remove(0);
        assert!(stored.is_in_review(&r1.id), "the first round stays attributed to r1");
        assert!(stored.is_in_review(&r2.id), "the reply puts the thread in r2 as well");
        assert_eq!(
            stored.comments.iter().map(|c| c.review_id.clone()).collect::<Vec<_>>(),
            vec![Some(r1.id.clone()), None, Some(r2.id.clone())],
            "turns are attributed per round; the agent reply carries no membership",
        );
        assert_eq!(stored.origin_review(), Some(r1.id.as_str()));
        assert_eq!(stored.latest_review(), Some(r2.id.as_str()));
    }

    #[test]
    fn sealing_a_new_turn_reopens_an_addressed_thread() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save");
        submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit 1");
        let status =
            append_reply(&db, "forge", "feat", "a", "implementer", "done", "").expect("reply");
        assert_eq!(status, ReviewStatus::Addressed, "the agent's answer addressed it");
        reply(&db, "a", "not quite");
        assert_eq!(
            find_thread_by_id(&db, "forge", "feat", "a").expect("load").expect("thread").status,
            ReviewStatus::Addressed,
            "typing a reply alone does not change state",
        );
        submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit 2");
        assert_eq!(
            find_thread_by_id(&db, "forge", "feat", "a").expect("load").expect("thread").status,
            ReviewStatus::Open,
            "sealing the reply hands the thread back to the agent",
        );
    }

    #[test]
    fn submitting_without_a_new_turn_leaves_an_addressed_thread_addressed() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10), thread("b", 20)]).expect("save");
        submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit 1");
        append_reply(&db, "forge", "feat", "a", "implementer", "done", "").expect("reply");
        // A later round that only files another thread must not disturb a
        // thread the agent has already answered.
        submit_review(&db, "forge", "feat", None, &["a".to_owned(), "b".to_owned()])
            .expect("submit 2");
        assert_eq!(
            find_thread_by_id(&db, "forge", "feat", "a").expect("load").expect("thread").status,
            ReviewStatus::Addressed,
            "no new turn on a, so its state is untouched",
        );
    }

    #[test]
    fn sealing_does_not_reopen_a_resolved_thread() {
        let (_dir, db) = open_db();
        save(&db, "forge", "feat", &[thread("a", 10)]).expect("save");
        submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit 1");
        reply(&db, "a", "one more thought");
        set_status(&db, "forge", "feat", "a", ReviewStatus::Resolved).expect("resolve");
        submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit 2");
        assert_eq!(
            find_thread_by_id(&db, "forge", "feat", "a").expect("load").expect("thread").status,
            ReviewStatus::Resolved,
            "the reviewer's own resolve outranks the reopen nudge",
        );
    }

    #[test]
    fn a_thread_filed_before_per_turn_membership_keeps_its_history() {
        // A row written while membership lived on the thread: its user turns
        // must read as part of that review, not as unfiled work that would
        // re-trip the submit gate.
        let (_dir, db) = open_db();
        let stored = serde_json::json!([{
            "id": "a",
            "anchor": {
                "path": "src/x.rs",
                "side": "New",
                "line": 10,
                "content_hash": 10,
                "context": ["ctx"],
                "base_ref": "main"
            },
            "comments": [
                { "author": "User", "text": "why?", "at": "2026-07-19T10:00:00Z" },
                {
                    "author": { "Agent": { "label": "implementer" } },
                    "text": "because",
                    "at": "2026-07-19T10:05:00Z"
                }
            ],
            "status": "Addressed",
            "created_at": "2026-07-19T10:00:00Z",
            "updated_at": "2026-07-19T10:05:00Z",
            "review_id": "review-1"
        }]);
        let txn = db.database().begin_write().expect("txn");
        {
            let mut table = txn.open_table(REVIEW_THREADS).expect("table");
            let bytes = serde_json::to_vec(&stored).expect("encode");
            table.insert(("forge", "feat"), bytes.as_slice()).expect("insert");
        }
        txn.commit().expect("commit");

        let loaded = load(&db, "forge", "feat").expect("load").remove(0);
        assert!(loaded.is_in_review("review-1"), "the thread still belongs to its review");
        assert_eq!(loaded.comments[0].review_id.as_deref(), Some("review-1"));
        assert_eq!(loaded.comments[1].review_id, None, "the agent reply gains no membership");
        assert!(!loaded.has_unfiled_user_turn(), "filed history does not read as pending work");
    }
}
