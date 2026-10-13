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
/// call's release: the same three endings a browser hand-off has, with
/// `Done` carrying what the finish filed.
///
/// **The parked call answers on `Done`, not on the open.** The reader
/// accepts, reads and comments, and the finish is what releases the call -
/// so the agent learns what the reading produced, and a reader who never
/// accepts releases it with `NotNow` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentHandOffEnding {
    /// The reader finished. `review_id` names the review the finish filed
    /// under the document's scope, absent when nothing was commented.
    Done { review_id: Option<String> },
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
/// with nothing to say about what the document is.
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
/// the detected kind, and whether the DOCUMENT is reviewable - markdown and
/// text are, a PDF or an unrecognised kind is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRead {
    /// The resolved absolute path - what a viewer deep-links by, and what
    /// the hand-off answers with.
    pub path: PathBuf,
    pub kind: DocumentKind,
    pub bytes: Vec<u8>,
    pub reviewable: bool,
    /// FNV-1a over the whole document's bytes, the same function a code
    /// anchor hashes a line with. A doc-scoped thread stores this in its
    /// anchor, so a document that changed under its comments reads outdated
    /// at once - nothing re-anchors a document automatically.
    pub content_hash: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

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
