//! Review threads and submitted reviews for one branch.

use forge_primitives::ReviewSet;
use forge_primitives::review::ReviewThread;

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

impl ViewSurface {
    pub fn reviews(&self, project: &str, branch: &str) -> ReviewsView {
        ReviewsView {
            threads: self.workspace.load_review_threads(project, branch),
            reviews: self.workspace.load_reviews(project, branch),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::review::{
        ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus,
    };
    use forge_workspace::SessionSlot;

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
}
