//! The document hand-off's types: what a hand-off names, and what a read
//! answers with.
//!
//! Type-only - the read that produces a [`DocumentRead`] lives in
//! `forge_agent::env::documents`. The kind crosses every layer (the read
//! detects it, the wire carries it, a viewer draws it), so it lives here per
//! the crate placement guide; the resolved path, bytes and reviewable flag
//! ride beside it rather than as separate parameters everywhere.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A parked hand-off as the read surface carries it: the id an answer
/// names, the resolved path the viewer opens at, and the reviewable flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentHandOffAsk {
    pub id: uuid::Uuid,
    pub path: String,
    pub reviewable: bool,
}

/// How a document hand-off ended, as the reader's answer and the parked
/// call's release.
///
/// **The parked call answers on a decision, not on the open.** The reader
/// accepts, reads and comments, and then chooses one of two outcomes:
/// `Approve` or `Feedback`. Both may carry a note and name the review the
/// finish filed. Approval is the one signal an agent may act on; feedback
/// means the document is revised and handed off again. A bare done is not
/// an outcome, so a reader who presses neither leaves the call parked; a
/// reader who never accepts releases it with `NotNow` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentHandOffEnding {
    /// The reader approved the document and the agent may proceed.
    /// `review_id` names the review the finish filed under the document's
    /// scope, absent when nothing was commented; `note` is the reader's
    /// own words, carried verbatim.
    Approve { review_id: Option<String>, note: Option<String> },
    /// The reader says something is off. The document is to be revised and
    /// handed off again - this is never approval, whatever it carries.
    /// `review_id` and `note` mean what `Approve`'s do.
    Feedback { review_id: Option<String>, note: Option<String> },
    /// The reader chose not to open it.
    NotNow,
    /// The call's waiter went away before any answer.
    Abandoned,
}

/// What a document is, as far as the read can tell - by extension, not by
/// sniffing the bytes.
///
/// `Other` is kept as itself rather than bucketed: a viewer draws the kind
/// it was handed, and folding every unknown into one variant would leave it
/// with nothing to say about what the document is. The Files reader's wider
/// UTF-8 drawing classification is the client's policy, not this kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Markdown,
    Pdf,
    Text,
    /// An extension the list does not know, lowercased and bare (`"rs"`),
    /// or empty when the name carries none.
    Other(String),
}

/// One document, as the read answered: the path it resolved to, the bytes,
/// the detected kind, and whether the DOCUMENT can carry a review - the
/// hand-off's commentable set, by extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRead {
    /// The resolved absolute path - what a viewer deep-links by, and what
    /// the hand-off answers with.
    pub path: PathBuf,
    pub kind: DocumentKind,
    pub bytes: Vec<u8>,
    pub reviewable: bool,
    /// FNV-1a over the whole document's bytes, the same function a code
    /// anchor hashes a line with. Threads are placed by their own anchors,
    /// re-found by their first captured line against the document a read
    /// carries; this is the read's own fact about the content it answered.
    pub content_hash: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The two answers cross as their own names, note and review
    /// included.** A reader's approval and a reader's feedback are the one
    /// signal the agent may act on and may not: an unknown or misread
    /// ending would let a revision be taken as done.
    #[test]
    fn the_two_answers_cross_as_their_own_names_with_their_note() {
        let approve = DocumentHandOffEnding::Approve {
            review_id: Some("r-1".to_owned()),
            note: Some("ship it".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(&approve).expect("approval encodes"),
            serde_json::json!({"approve": {"review_id": "r-1", "note": "ship it"}}),
        );
        let feedback = DocumentHandOffEnding::Feedback { review_id: None, note: None };
        assert_eq!(
            serde_json::to_value(&feedback).expect("feedback encodes"),
            serde_json::json!({"feedback": {"review_id": null, "note": null}}),
        );
        let back: DocumentHandOffEnding = serde_json::from_value(
            serde_json::json!({"feedback": {"review_id": "r-2", "note": "no"}}),
        )
        .expect("feedback decodes");
        assert_eq!(
            back,
            DocumentHandOffEnding::Feedback {
                review_id: Some("r-2".to_owned()),
                note: Some("no".to_owned()),
            },
        );
    }

    /// A kind the list does not know crosses as itself, and one with no
    /// extension as an empty name - the two shapes a viewer's narrowing has
    /// to read, so they are pinned here rather than only in the envelope.
    #[test]
    fn other_kinds_cross_as_their_own_name() {
        let known = serde_json::to_value(DocumentKind::Markdown).expect("a kind encodes");
        assert_eq!(known, serde_json::json!("markdown"));

        let other = serde_json::to_value(DocumentKind::Other("rs".to_owned())).expect("encodes");
        assert_eq!(other, serde_json::json!({ "other": "rs" }));

        let back: DocumentKind = serde_json::from_value(other).expect("decodes");
        assert_eq!(back, DocumentKind::Other("rs".to_owned()));
    }
}
