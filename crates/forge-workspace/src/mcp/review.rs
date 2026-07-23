//! Review-conversation tools (Phase 2).
//!
//! Worker-facing MCP tools that turn a submitted review set into a
//! two-way conversation. The reviewer submits comments via `/diff`; the
//! worker reads them, replies, and resolves through
//! `mcp__forge__review__*`, and each reply appends to the comment thread
//! and flips it `Open` -> `Addressed`.
//!
//! This module holds the JSON view shapes the tools return plus the pure
//! mappers from the durable [`forge_primitives::review`] types. The store
//! I/O lives in [`crate::store::review`]; the `Workspace::review_*`
//! wrappers surface it; the `Tool` impls + caller resolution live beside
//! this (the `facade` submodule and the tool structs).

use serde::Serialize;

use forge_primitives::review::{ReviewAuthor, ReviewSet, ReviewSide, ReviewStatus, ReviewThread};

/// One row of `review__list`: a submitted review plus its member-comment
/// tally by state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewSummary {
    pub review_id: String,
    pub number: u32,
    pub summary: Option<String>,
    pub created_at: String,
    pub comment_count: usize,
    pub open: usize,
    pub addressed: usize,
    pub resolved: usize,
    pub outdated: usize,
}

/// `review__get`: a review's overview plus the comments filed under it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewDetail {
    pub review_id: String,
    pub number: u32,
    pub summary: Option<String>,
    pub comments: Vec<ReviewCommentView>,
}

/// One comment in `review__get`, carrying the anchored code so the worker
/// can locate the spot regardless of the comment's diff scope (a
/// commit-scoped comment's line may not match the current working tree).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewCommentView {
    pub comment_id: String,
    pub file: String,
    pub line: u32,
    pub side: &'static str,
    pub status: &'static str,
    /// The captured lines around the anchored spot
    /// ([`forge_primitives::review::ReviewAnchor::context`]), so a shifted
    /// line number still locates the code the comment refers to.
    pub context: Vec<String>,
    pub thread: Vec<ReviewTurnView>,
}

/// One turn in a comment thread: `author` is `"you"` for the reviewer or
/// the agent's label for a worker reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewTurnView {
    pub author: String,
    pub text: String,
    pub at: String,
}

fn side_str(side: ReviewSide) -> &'static str {
    match side {
        ReviewSide::Old => "old",
        ReviewSide::New => "new",
    }
}

fn status_str(status: ReviewStatus) -> &'static str {
    match status {
        ReviewStatus::Open => "open",
        ReviewStatus::Addressed => "addressed",
        ReviewStatus::Resolved => "resolved",
        ReviewStatus::Outdated => "outdated",
    }
}

fn author_str(author: &ReviewAuthor) -> String {
    match author {
        ReviewAuthor::User => "you".to_owned(),
        ReviewAuthor::Agent { label } => label.clone(),
    }
}

/// Build the `review__list` rows for `reviews` given every thread on the
/// branch, newest review first. A review's members are the threads whose
/// `review_id` points at it.
pub(crate) fn summarize(reviews: &[ReviewSet], threads: &[ReviewThread]) -> Vec<ReviewSummary> {
    reviews
        .iter()
        .rev()
        .map(|review| {
            let (mut open, mut addressed, mut resolved, mut outdated) = (0, 0, 0, 0);
            let mut count = 0;
            for t in threads.iter().filter(|t| t.review_id.as_deref() == Some(&review.id)) {
                count += 1;
                match t.status {
                    ReviewStatus::Open => open += 1,
                    ReviewStatus::Addressed => addressed += 1,
                    ReviewStatus::Resolved => resolved += 1,
                    ReviewStatus::Outdated => outdated += 1,
                }
            }
            ReviewSummary {
                review_id: review.id.clone(),
                number: review.number,
                summary: review.summary.clone(),
                created_at: review.created_at.clone(),
                comment_count: count,
                open,
                addressed,
                resolved,
                outdated,
            }
        })
        .collect()
}

/// Build the `review__get` detail for `review_id`, or `None` when no such
/// review exists on the branch.
pub(crate) fn detail(
    reviews: &[ReviewSet],
    threads: &[ReviewThread],
    review_id: &str,
) -> Option<ReviewDetail> {
    let review = reviews.iter().find(|r| r.id == review_id)?;
    let comments = threads
        .iter()
        .filter(|t| t.review_id.as_deref() == Some(review_id))
        .map(comment_view)
        .collect();
    Some(ReviewDetail {
        review_id: review.id.clone(),
        number: review.number,
        summary: review.summary.clone(),
        comments,
    })
}

fn comment_view(thread: &ReviewThread) -> ReviewCommentView {
    ReviewCommentView {
        comment_id: thread.id.clone(),
        file: thread.anchor.path.clone(),
        line: thread.anchor.line,
        side: side_str(thread.anchor.side),
        status: status_str(thread.status),
        context: thread.anchor.context.clone(),
        thread: thread
            .comments
            .iter()
            .map(|c| ReviewTurnView {
                author: author_str(&c.author),
                text: c.text.clone(),
                at: c.at.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::review::{ReviewAnchor, ReviewComment};

    fn review(id: &str, number: u32, summary: Option<&str>) -> ReviewSet {
        ReviewSet {
            id: id.to_owned(),
            number,
            summary: summary.map(str::to_owned),
            created_at: "2026-07-23T10:00:00Z".to_owned(),
        }
    }

    fn thread(id: &str, review_id: Option<&str>, status: ReviewStatus) -> ReviewThread {
        ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 42,
                content_hash: 7,
                context: vec!["fn f() {".to_owned(), "    todo!()".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: format!("look at {id}"),
                at: "2026-07-23T10:00:00Z".to_owned(),
            }],
            status,
            created_at: "2026-07-23T10:00:00Z".to_owned(),
            updated_at: "2026-07-23T10:00:00Z".to_owned(),
            commit: None,
            review_id: review_id.map(str::to_owned),
        }
    }

    #[test]
    fn summarize_tallies_members_newest_first() {
        let reviews = vec![review("r1", 1, Some("first")), review("r2", 2, None)];
        let threads = vec![
            thread("a", Some("r1"), ReviewStatus::Open),
            thread("b", Some("r1"), ReviewStatus::Addressed),
            thread("c", Some("r1"), ReviewStatus::Resolved),
            thread("d", Some("r2"), ReviewStatus::Outdated),
            thread("e", None, ReviewStatus::Open),
        ];
        let rows = summarize(&reviews, &threads);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].review_id, "r2", "newest review leads");
        assert_eq!(rows[0].comment_count, 1);
        assert_eq!(rows[0].outdated, 1);
        assert_eq!(rows[1].review_id, "r1");
        assert_eq!(rows[1].comment_count, 3, "the unfiled thread does not tally into r1");
        assert_eq!(rows[1].open, 1);
        assert_eq!(rows[1].addressed, 1);
        assert_eq!(rows[1].resolved, 1);
        assert_eq!(rows[1].summary.as_deref(), Some("first"));
    }

    #[test]
    fn detail_maps_comments_with_anchor_context() {
        let reviews = vec![review("r1", 1, Some("overview"))];
        let mut t = thread("a", Some("r1"), ReviewStatus::Addressed);
        t.anchor.side = ReviewSide::Old;
        t.comments.push(ReviewComment {
            author: ReviewAuthor::Agent { label: "implementer".to_owned() },
            text: "done".to_owned(),
            at: "2026-07-23T11:00:00Z".to_owned(),
        });
        let got = detail(&reviews, &[t], "r1").expect("review found");
        assert_eq!(got.number, 1);
        assert_eq!(got.summary.as_deref(), Some("overview"));
        assert_eq!(got.comments.len(), 1);
        let c = &got.comments[0];
        assert_eq!(c.comment_id, "a");
        assert_eq!(c.file, "src/x.rs");
        assert_eq!(c.line, 42);
        assert_eq!(c.side, "old");
        assert_eq!(c.status, "addressed");
        assert_eq!(c.context, vec!["fn f() {".to_owned(), "    todo!()".to_owned()]);
        assert_eq!(c.thread.len(), 2, "user comment + agent reply");
        assert_eq!(c.thread[0].author, "you");
        assert_eq!(c.thread[1].author, "implementer");
        assert_eq!(c.thread[1].text, "done");
    }

    #[test]
    fn detail_is_none_for_unknown_review() {
        let reviews = vec![review("r1", 1, None)];
        assert!(detail(&reviews, &[], "nope").is_none());
    }
}
