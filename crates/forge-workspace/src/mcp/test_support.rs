//! What the family tests need to read out of a tool output.
//!
//! A content block is text or an image rather than a text field, so a test
//! asserting on what a tool said reads the text through here. The panics are
//! the test's own failure to say which: reaching for text and finding a
//! picture is the wrong question, not a value to return.

use forge_sdk::mcp::tool::{ToolOutput, ToolOutputBlock};

/// The text of `output`'s first block.
pub(crate) fn text_of(output: &ToolOutput) -> &str {
    let Some(block) = output.blocks.first() else {
        panic!("text_of: the output carries no block at all");
    };
    block_text(block)
}

/// The text of one block.
pub(crate) fn block_text(block: &ToolOutputBlock) -> &str {
    match block {
        ToolOutputBlock::Text { text } => text,
        ToolOutputBlock::Image { .. } => panic!("block_text: this block is an image"),
    }
}
