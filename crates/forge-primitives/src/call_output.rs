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
