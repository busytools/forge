//! `reviews()`: the review threads and submitted reviews for one branch.

use forge_primitives::review::ReviewThread;
use forge_primitives::{ReviewSet, SessionSlot};
use forge_workspace::env::git_diff::resolver::{AnchorPlacement, place_anchor_in_lines};

use super::ViewSurface;

/// Everything the `/diff` overlay reads for one `(project, branch)`.
///
/// Two Results rather than one, because the two live in separate redb
/// tables (`review_threads` and `reviews`) and fail independently
/// today. Collapsing them would let a corrupt reviews row start failing
/// a threads-only reader.
pub struct ReviewsView {
    pub threads: Result<Vec<ReviewThread>, String>,
    pub reviews: Result<Vec<ReviewSet>, String>,
}

/// One seat-tree thread and where its anchor sits in the seat's current
/// file, or `None` when no landing could be taken - the file the comment
/// was written on could not be read, or the seat's tree has no cwd. A
/// thread with no landing is CARRIED, never dropped; the page parks it.
pub struct TreeThreadView {
    pub thread: ReviewThread,
    pub placement: Option<AnchorPlacement>,
}

/// The files page's review half: the seat-tree scope's threads, each
/// placed against the seat's own working files, plus its review sets.
///
/// The scope key is the seat (`tree_scope_ref`), so the scope never
/// changes with the branch a worker happens to be on.
pub struct TreeReviewsView {
    pub scope: String,
    pub threads: Result<Vec<TreeThreadView>, String>,
    pub reviews: Result<Vec<ReviewSet>, String>,
}

impl ViewSurface {
    pub fn reviews(&self, project: &str, branch: &str) -> ReviewsView {
        ReviewsView {
            threads: self.workspace.load_review_threads(project, branch),
            reviews: self.workspace.load_reviews(project, branch),
        }
    }

    /// The seat's tree-scope reviews, each thread placed against the seat's
    /// working files by line content and neighbours - one bounded read per
    /// commented file, zero threads meaning zero reads.
    pub fn tree_reviews(&self, seat: &SessionSlot) -> TreeReviewsView {
        let scope = forge_primitives::review::tree_scope_ref(seat);
        let view = self.reviews(seat.project(), &scope);
        let threads = view.threads.map(|threads| self.place_tree_threads(seat, threads));
        TreeReviewsView { scope, threads, reviews: view.reviews }
    }

    /// Place every seated thread by line content and neighbours against
    /// the file it was written on, read from the seat's tree NOW.
    ///
    /// Each commented file is read once per call, through the same
    /// bounded document read a viewer's route uses (the ceiling, the
    /// escape check, the UTF-8 answer); zero threads means zero reads. A
    /// file the read refused leaves its threads unplaced rather than
    /// dropping them - a vanished file must not erase the comments about
    /// it.
    fn place_tree_threads(
        &self,
        seat: &SessionSlot,
        threads: Vec<ReviewThread>,
    ) -> Vec<TreeThreadView> {
        let cwd = self.roster().cwd_for(seat);
        let ceiling = self.workspace.document_max_bytes();
        let mut files: std::collections::HashMap<String, Option<Vec<String>>> =
            std::collections::HashMap::new();
        threads
            .into_iter()
            .map(|thread| {
                let placement = cwd.as_ref().and_then(|cwd| {
                    let lines = files.entry(thread.anchor.path.clone()).or_insert_with(|| {
                        forge_workspace::env::documents::read(
                            cwd,
                            &thread.anchor.path,
                            false,
                            ceiling,
                        )
                        .ok()
                        .and_then(|read| String::from_utf8(read.bytes).ok())
                        .map(|text| text.lines().map(str::to_owned).collect())
                    });
                    lines.as_ref().map(|lines| place_anchor_in_lines(&thread.anchor, lines))
                });
                TreeThreadView { thread, placement }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::review::{
        ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus,
    };
    use forge_workspace::{SessionSlot, Workspace};

    use super::*;

    fn thread(id: &str, line: u32) -> ReviewThread {
        ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/lib.rs".to_owned(),
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
        }
    }

    /// Catches the two halves being wired to each other's table, or one
    /// half being dropped - either would show threads where review sets
    /// belong in the diff overlay's list.
    #[test]
    fn reviews_agrees_with_the_calls_it_replaces() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        assert!(
            workspace.upsert_review_thread("forge", "feat", thread("a", 10)),
            "the seeded thread was stored",
        );
        workspace
            .submit_review(
                "forge",
                "feat",
                Some("first pass".to_owned()),
                &["a".to_owned()],
                SessionSlot::from_str_for_test("reviewer"),
            )
            .expect("seeded review set");

        let view = ViewSurface::new(Arc::clone(&workspace)).reviews("forge", "feat");

        assert_eq!(
            view.threads,
            workspace.load_review_threads("forge", "feat"),
            "the threads half is the call's own",
        );
        assert_eq!(
            view.reviews,
            workspace.load_reviews("forge", "feat"),
            "the reviews half is the call's own",
        );
        assert_eq!(
            view.threads.as_ref().map(Vec::len),
            Ok(1),
            "the seeded thread is what the threads half returns",
        );
        assert_eq!(
            view.reviews.as_ref().map(Vec::len),
            Ok(1),
            "the seeded review set is what the reviews half returns",
        );
    }

    /// The seat-tree half places each thread against the seat's WORKING
    /// files: in place when the recorded line still carries the comment's
    /// text, moved when the file shifted under it, and no landing at all -
    /// never a drop, never a guess - when the file cannot be read.
    #[test]
    fn tree_reviews_places_threads_against_the_seats_working_files() {
        use forge_primitives::review::tree_scope_ref;
        use forge_workspace::env::git_diff::resolver::{AnchorPlacement, anchor_hash};

        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _updates) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).expect("project dir");
        workspace.seed_test_project("forge", &root.to_string_lossy());
        workspace.install_db_for_test(
            forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        let worker = SessionSlot::worker("TestOrg", "forge", "reader");
        let project = ViewSurface::new(Arc::clone(&workspace))
            .roster()
            .project_named("forge")
            .expect("seeded project")
            .key
            .clone();
        workspace.insert_live_worker(
            &project,
            forge_workspace::WorkerEntry {
                label: "reader".to_owned(),
                charter: "charter".to_owned(),
                slot: worker.clone(),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead("TestOrg", "forge"),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: None,
                diagnostic: None,
                kick: None,
            },
        );

        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("src dir");
        let file = src.join("x.rs");
        std::fs::write(&file, "fn f() {\n    let a = 1;\n    let b = 2;\n}\n").expect("write");

        let scope = tree_scope_ref(&worker);
        let mut seeded = thread("t1", 2);
        seeded.anchor.path = "src/x.rs".to_owned();
        seeded.anchor.content_hash = anchor_hash("    let a = 1;");
        seeded.anchor.context = vec!["fn f() {".to_owned(), "    let b = 2;".to_owned()];
        assert!(workspace.upsert_review_thread("forge", &scope, seeded), "seeded thread");

        let view = ViewSurface::new(Arc::clone(&workspace)).tree_reviews(&worker);
        assert_eq!(view.scope, scope, "the read's scope is the seat's tree");
        let threads = view.threads.expect("the threads read answered");
        assert_eq!(threads.len(), 1);
        assert_eq!(
            threads[0].placement,
            Some(AnchorPlacement::Placed { line: 2, moved_from: None }),
            "in place against the working file",
        );

        // The file shifts under the comment: the landing moves with it.
        std::fs::write(&file, "// header\nfn f() {\n    let a = 1;\n    let b = 2;\n}\n")
            .expect("rewrite");
        let view = ViewSurface::new(Arc::clone(&workspace)).tree_reviews(&worker);
        let threads = view.threads.expect("the threads read answered");
        assert_eq!(
            threads[0].placement,
            Some(AnchorPlacement::Placed { line: 3, moved_from: Some(2) }),
            "the shifted line is re-found by content and neighbours",
        );

        // The file vanishes: the thread is carried with no landing.
        std::fs::remove_file(&file).expect("remove");
        let view = ViewSurface::new(Arc::clone(&workspace)).tree_reviews(&worker);
        let threads = view.threads.expect("the threads read answered");
        assert_eq!(threads.len(), 1, "the thread survives an unreadable file");
        assert_eq!(threads[0].placement, None, "no landing rather than a guess");
    }
}
