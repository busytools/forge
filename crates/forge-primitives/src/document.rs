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
/// the detected kind, and the reviewable flag the hand-off passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRead {
    /// The resolved absolute path - what a viewer deep-links by, and what
    /// the hand-off answers with.
    pub path: PathBuf,
    pub kind: DocumentKind,
    pub bytes: Vec<u8>,
    pub reviewable: bool,
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
