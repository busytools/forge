//! A call's own output, as the read answers it.

use serde::{Deserialize, Serialize};

/// The answer to a read of a call's own output file: the tail of the file
/// the CLI streamed a backgrounded task's output to, or the reason there is
/// nothing to draw - never a blank.
///
/// The two reasons are named apart because a reader can act on the
/// difference: a call the conversation no longer names is not a file that
/// went away, and a view saying which of the two it met is the whole of what
/// this carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutput {
    /// The tail, oldest first, as the command wrote it - escape sequences
    /// and all. What a renderer drops is the renderer's rule: a terminal and
    /// a page drop different bytes.
    Lines(Vec<String>),
    /// The file the read was pointed at is gone, or would not open.
    FileGone,
    /// No output file is recorded for this call: the frame that named one is
    /// not in the conversation the task carries.
    NoPath,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape the client narrows, held from the Rust side: a name for
    /// the lines and a bare word for each reason. No compiler links the two
    /// sides, so this is what keeps serde's output and the client's
    /// `callOutputOf` together - the `draftEndingLine` precedent.
    #[test]
    fn a_call_output_serialises_as_the_client_narrows_it() {
        assert_eq!(
            serde_json::to_value(CallOutput::Lines(vec!["one".to_owned()])).expect("encodes"),
            serde_json::json!({ "lines": ["one"] }),
        );
        assert_eq!(
            serde_json::to_value(CallOutput::FileGone).expect("encodes"),
            serde_json::json!("file_gone"),
        );
        assert_eq!(
            serde_json::to_value(CallOutput::NoPath).expect("encodes"),
            serde_json::json!("no_path"),
        );
    }
}
