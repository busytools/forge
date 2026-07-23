//! Durable local-review wire types.
//!
//! A review thread is a comment anchored to a diff line in the `/diff`
//! overlay, persisted per `(project, branch)` so it survives overlay
//! close and forge restarts. Pure data shapes; the redb persistence
//! lives in `forge-workspace::store::review` and the re-anchoring in
//! `forge-agent::env::git_diff`.

use serde::{Deserialize, Serialize};

/// Lifecycle state of a review thread. A worker reply flips `Open` ->
/// `Addressed`; only the user ever `Resolved`s; `Outdated` marks a
/// thread whose anchored line drifted out from under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    Open,
    Addressed,
    Resolved,
    Outdated,
}

/// Which side of the diff the anchored line sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewSide {
    Old,
    New,
}

/// Who wrote a review comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewAuthor {
    User,
    Agent { label: String },
}

/// One comment in a thread; `at` is an rfc3339 timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub author: ReviewAuthor,
    pub text: String,
    pub at: String,
}

/// Durable, drift-robust location of a review thread, re-resolved to a
/// positional index against a fresh diff scan on every overlay open;
/// `content_hash` + `context` let the resolver re-anchor a moved line
/// or flag it `Outdated`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAnchor {
    pub path: String,
    pub side: ReviewSide,
    pub line: u32,
    pub content_hash: u64,
    pub context: Vec<String>,
    pub base_ref: String,
}

/// A persisted review thread: an anchor plus its comment chain and
/// lifecycle state; `created_at` / `updated_at` are rfc3339. `commit` is
/// the sha the thread was authored under (`None` = whole-diff scope);
/// `#[serde(default)]` loads pre-scope threads as whole-diff. `review_id`
/// is the [`ReviewSet`] this comment was filed into (`None` = unfiled,
/// not yet in a submitted review); `#[serde(default)]` loads pre-sets
/// threads as unfiled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    pub id: String,
    pub anchor: ReviewAnchor,
    pub comments: Vec<ReviewComment>,
    pub status: ReviewStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub review_id: Option<String>,
}

/// A submitted review: the sealed grouping of the comments filed under
/// one pass, plus an optional overview. Persisted per `(project, branch)`.
/// `number` is the human-facing 1-based sequence (`R1`, `R2`, ...) minted
/// as the branch's review count + 1; `id` is the stable key threads
/// carry in [`ReviewThread::review_id`]. `created_at` is rfc3339.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSet {
    pub id: String,
    pub number: u32,
    pub summary: Option<String>,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_thread() -> ReviewThread {
        ReviewThread {
            id: "thread-1".to_owned(),
            anchor: ReviewAnchor {
                path: "crates/forge-workspace/src/workspace.rs".to_owned(),
                side: ReviewSide::New,
                line: 4781,
                content_hash: 0xdead_beef_cafe_f00d,
                context: vec![
                    "prev line".to_owned(),
                    "anchored".to_owned(),
                    "next line".to_owned(),
                ],
                base_ref: "main".to_owned(),
            },
            comments: vec![
                ReviewComment {
                    author: ReviewAuthor::User,
                    text: "does this fire for the failure-notice path too?".to_owned(),
                    at: "2026-07-19T10:00:00Z".to_owned(),
                },
                ReviewComment {
                    author: ReviewAuthor::Agent { label: "implementer".to_owned() },
                    text: "yes, test added".to_owned(),
                    at: "2026-07-19T10:05:00Z".to_owned(),
                },
            ],
            status: ReviewStatus::Resolved,
            created_at: "2026-07-19T10:00:00Z".to_owned(),
            updated_at: "2026-07-19T10:05:00Z".to_owned(),
            commit: None,
            review_id: None,
        }
    }

    #[test]
    fn review_thread_round_trips_through_json() {
        let thread = sample_thread();
        let json = serde_json::to_string(&thread).expect("serialize");
        let back: ReviewThread = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(thread, back);
    }

    #[test]
    fn large_content_hash_survives_json_round_trip() {
        // u64 above 2^53 must not lose precision through serde_json.
        let mut thread = sample_thread();
        thread.anchor.content_hash = u64::MAX;
        let json = serde_json::to_string(&thread).expect("serialize");
        let back: ReviewThread = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.anchor.content_hash, u64::MAX);
    }

    #[test]
    fn commit_scoped_thread_round_trips() {
        let mut thread = sample_thread();
        thread.commit = Some("abc123".to_owned());
        let json = serde_json::to_string(&thread).expect("serialize");
        let back: ReviewThread = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.commit, Some("abc123".to_owned()));
    }

    #[test]
    fn thread_without_commit_key_defaults_to_none() {
        // A whole-diff thread persisted before the scope field existed has
        // no `commit` key on disk; it must load as `None`, not error.
        let json = r#"{
            "id": "t1",
            "anchor": {
                "path": "a.rs",
                "side": "New",
                "line": 1,
                "content_hash": 0,
                "context": [],
                "base_ref": "main"
            },
            "comments": [],
            "status": "Open",
            "created_at": "2026-07-19T10:00:00Z",
            "updated_at": "2026-07-19T10:00:00Z"
        }"#;
        let back: ReviewThread = serde_json::from_str(json).expect("deserialize legacy thread");
        assert_eq!(back.commit, None);
    }

    #[test]
    fn review_id_defaults_to_none_when_absent() {
        // A thread persisted before review-sets has no `review_id` key on
        // disk; it must load as unfiled (`None`), not error.
        let json = r#"{
            "id": "t1",
            "anchor": {
                "path": "a.rs",
                "side": "New",
                "line": 1,
                "content_hash": 0,
                "context": [],
                "base_ref": "main"
            },
            "comments": [],
            "status": "Open",
            "created_at": "2026-07-19T10:00:00Z",
            "updated_at": "2026-07-19T10:00:00Z"
        }"#;
        let back: ReviewThread = serde_json::from_str(json).expect("deserialize pre-sets thread");
        assert_eq!(back.review_id, None);
    }

    #[test]
    fn filed_thread_round_trips_its_review_id() {
        let mut thread = sample_thread();
        thread.review_id = Some("review-7".to_owned());
        let json = serde_json::to_string(&thread).expect("serialize");
        let back: ReviewThread = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.review_id, Some("review-7".to_owned()));
    }

    #[test]
    fn review_set_round_trips_through_json() {
        let review = ReviewSet {
            id: "review-1".to_owned(),
            number: 3,
            summary: Some("Two nits on error handling.".to_owned()),
            created_at: "2026-07-23T10:00:00Z".to_owned(),
        };
        let json = serde_json::to_string(&review).expect("serialize");
        let back: ReviewSet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(review, back);
    }

    #[test]
    fn review_set_without_summary_round_trips() {
        let review = ReviewSet {
            id: "review-2".to_owned(),
            number: 1,
            summary: None,
            created_at: "2026-07-23T10:00:00Z".to_owned(),
        };
        let json = serde_json::to_string(&review).expect("serialize");
        let back: ReviewSet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.summary, None);
    }

    #[test]
    fn all_status_variants_round_trip() {
        for status in [
            ReviewStatus::Open,
            ReviewStatus::Addressed,
            ReviewStatus::Resolved,
            ReviewStatus::Outdated,
        ] {
            let json = serde_json::to_string(&status).expect("serialize");
            let back: ReviewStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(status, back);
        }
    }

    #[test]
    fn old_side_and_user_author_round_trip() {
        let anchor = ReviewAnchor {
            path: "a.rs".to_owned(),
            side: ReviewSide::Old,
            line: 12,
            content_hash: 7,
            context: Vec::new(),
            base_ref: "HEAD".to_owned(),
        };
        let json = serde_json::to_string(&anchor).expect("serialize");
        let back: ReviewAnchor = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(anchor, back);
        assert_eq!(back.side, ReviewSide::Old);
    }
}
