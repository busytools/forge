//! Review-thread persistence on the redb `review_threads` table.
//!
//! The `/diff` overlay's line comments persist here per `(project,
//! branch)` so a review round survives closing the overlay and forge
//! restarts. Keyed by `(project, branch)` - the whole set for a branch
//! is one serde-json blob. `project` is the forge.toml project NAME
//! (worktree-agnostic), never the directory-hash key.

use anyhow::Context;
use forge_primitives::review::{ReviewSet, ReviewStatus, ReviewThread};
use redb::{ReadableTable, TableDefinition};

use super::Db;

const REVIEW_THREADS: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("review_threads");
/// Submitted reviews per `(project, branch)` - the sealed groupings a
/// thread's `review_id` points into. Sibling of [`REVIEW_THREADS`]; the
/// whole set for a branch is one serde-json blob.
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

/// Seal a new review for `(project, branch)`: mint its number (existing
/// count + 1), stamp each listed thread that is still unfiled with the
/// new review id, and append the [`ReviewSet`]. Threads not in
/// `thread_ids` and threads already filed are untouched (a filed comment
/// never moves). One write transaction spans both tables so the stamp
/// and the append land together.
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
                for thread in &mut threads {
                    if thread.review_id.is_none() && thread_ids.contains(&thread.id) {
                        thread.review_id = Some(review.id.clone());
                        changed = true;
                    }
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
/// diagnosable rather than a bare serde message.
fn decode(bytes: &[u8], project: &str, branch: &str) -> anyhow::Result<Vec<ReviewThread>> {
    serde_json::from_slice(bytes)
        .with_context(|| format!("decode review threads for ({project}, {branch})"))
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
            }],
            status: ReviewStatus::Open,
            created_at: "2026-07-19T10:00:00Z".to_owned(),
            updated_at: "2026-07-19T10:00:00Z".to_owned(),
            commit: None,
            review_id: None,
        }
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
                .review_id
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
    fn submit_review_leaves_an_already_filed_thread_untouched() {
        let (_dir, db) = open_db();
        let mut a = thread("a", 10);
        a.review_id = Some("prior".to_owned());
        save(&db, "forge", "feat", &[a]).expect("save");
        let r = submit_review(&db, "forge", "feat", None, &["a".to_owned()]).expect("submit");
        assert_eq!(
            load(&db, "forge", "feat").expect("load")[0].review_id,
            Some("prior".to_owned()),
            "an already-filed thread is never re-filed",
        );
        assert_eq!(r.number, 1, "the review still mints even if it files nothing new");
    }
}
