use super::super::{
    App, AppStatus, ChatMessage, MessageBlock, MessageRole, TextBlock, TextBlockSpacing,
    TextSplitDecision, TextSplitKind, default_cache_split_policy, find_text_split,
};
use crate::agent::model;

pub(super) fn handle_agent_message_chunk(app: &mut App, chunk: model::ContentChunk) {
    let model::ContentBlock::Text(text) = chunk.content else {
        return;
    };

    app.status = AppStatus::Running;
    if text.text.is_empty() {
        return;
    }
    if let Some(owner_idx) = app.active_turn_assistant_idx()
        && let Some(owner) = app.active_messages_mut().get_mut(owner_idx)
    {
        append_agent_stream_text(&mut owner.blocks, &text.text);
        app.sync_after_message_blocks_changed(owner_idx);
        return;
    }

    // No active turn bound  -  this chunk belongs to a NEW assistant
    // turn (e.g. a Monitor / Task notification firing after the
    // previous turn finalised). Push a fresh assistant message rather
    // than appending to whatever assistant message happens to be
    // last, which would glue two unrelated turns together with no
    // separator (e.g. "...pull/107Monitor closed cleanly.").
    let mut blocks = Vec::new();
    append_agent_stream_text(&mut blocks, &text.text);
    app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, blocks, None));
    app.bind_active_turn_assistant_to_tail();
}

pub(super) fn append_agent_stream_text(blocks: &mut Vec<MessageBlock>, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    if let Some(MessageBlock::Text(block)) = blocks.last_mut() {
        block.text.push_str(chunk);
        block.markdown.append(chunk);
        block.cache.invalidate();
    } else {
        blocks.push(new_text_block(chunk.to_owned()));
    }

    let split_count = split_tail_text_block(blocks);
    if split_count > 0 {
        crate::perf::mark_with("text_block_split_count", "count", split_count);
    }

    if let Some(MessageBlock::Text(block)) = blocks.last() {
        crate::perf::mark_with("text_block_active_tail_bytes", "bytes", block.text.len());
    }
    let text_block_count = blocks.iter().filter(|b| matches!(b, MessageBlock::Text(..))).count();
    crate::perf::mark_with("text_block_frozen_count", "count", text_block_count.saturating_sub(1));
}

fn new_text_block(text: String) -> MessageBlock {
    MessageBlock::Text(TextBlock::new(text))
}

fn split_tail_text_block(blocks: &mut Vec<MessageBlock>) -> usize {
    let mut split_count = 0usize;
    #[allow(clippy::while_let_loop)] // multiple early-break conditions inside
    loop {
        let Some(tail_idx) = blocks.len().checked_sub(1) else {
            break;
        };
        let Some(split) = blocks.get(tail_idx).and_then(|block| {
            if let MessageBlock::Text(block) = block {
                find_text_block_split(block.text.as_str())
            } else {
                None
            }
        }) else {
            break;
        };

        let (completed, remainder) = match blocks.get(tail_idx) {
            Some(MessageBlock::Text(block)) => {
                (block.text[..split.split_at].to_owned(), block.text[split.split_at..].to_owned())
            }
            _ => break,
        };

        if completed.is_empty() || remainder.is_empty() {
            break;
        }

        blocks[tail_idx] = new_text_block(remainder);
        blocks.insert(tail_idx, completed_text_block(completed, split));
        split_count += 1;
    }
    split_count
}

fn completed_text_block(text: String, split: TextSplitDecision) -> MessageBlock {
    let trailing_spacing = match split.kind {
        TextSplitKind::Generic => TextBlockSpacing::None,
        TextSplitKind::ParagraphBoundary => TextBlockSpacing::ParagraphBreak,
    };
    MessageBlock::Text(TextBlock::new(text).with_trailing_spacing(trailing_spacing))
}

pub(super) fn find_text_block_split(text: &str) -> Option<TextSplitDecision> {
    find_text_split(text, *default_cache_split_policy())
}

#[cfg(test)]
pub(super) fn find_text_block_split_index(text: &str) -> Option<usize> {
    find_text_block_split(text).map(|decision| decision.split_at)
}
