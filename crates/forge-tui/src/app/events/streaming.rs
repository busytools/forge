use super::super::{
    App, AppStatus, ChatMessage, MessageBlock, MessageRole, TextBlock, TextBlockSpacing,
    TextSplitDecision, TextSplitKind, default_cache_split_policy, find_text_split,
};
use crate::agent::model;

pub(super) fn handle_agent_message_chunk(app: &mut App, chunk: model::ContentChunk) {
    let model::RenderContentBlock::Text(text) = chunk.content else {
        return;
    };

    app.status = AppStatus::Running;
    if text.text.is_empty() {
        return;
    }
    // Cloned out before the message borrow: the store handle is needed while
    // `owner` holds the messages.
    let render_caches = std::rc::Rc::clone(&app.render_caches);
    if let Some(owner_idx) = app.active_turn_assistant_idx()
        && let Some(owner) =
            app.active_messages_mut().and_then(|messages| messages.get_mut(owner_idx))
    {
        append_agent_stream_text(&mut owner.blocks, Some(&render_caches), &text.text);
        app.sync_after_message_tail_changed(owner_idx);
        return;
    }

    // No active turn bound - this chunk belongs to a NEW assistant
    // turn (e.g. a Monitor / Task notification firing after the
    // previous turn finalised). Push a fresh assistant message rather
    // than appending to whatever assistant message happens to be
    // last, which would glue two unrelated turns together with no
    // separator (e.g. "...pull/107Monitor closed cleanly.").
    let mut blocks = Vec::new();
    append_agent_stream_text(&mut blocks, Some(&app.render_caches), &text.text);
    app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, blocks));
    app.bind_active_turn_assistant_to_tail();
}

pub(super) fn append_agent_stream_text(
    blocks: &mut Vec<MessageBlock>,
    render_caches: Option<&crate::app::RenderCacheStore>,
    chunk: &str,
) {
    if chunk.is_empty() {
        return;
    }
    if let Some(MessageBlock::Text(block)) = blocks.last_mut() {
        block.text.push_str(chunk);
        // The id keys the markdown cache, so this extends the entry rather
        // than starting a new one - which is what keeps the incremental
        // render incremental.
        if let Some(render_caches) = render_caches {
            render_caches.markdown(block.id, &block.text).append(chunk);
        }
    } else {
        blocks.push(new_text_block(chunk.to_owned()));
    }

    let split_count = split_tail_text_block(blocks, render_caches);
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

fn split_tail_text_block(
    blocks: &mut Vec<MessageBlock>,
    render_caches: Option<&crate::app::RenderCacheStore>,
) -> usize {
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

        // The replaced block's id is dropped here, so its entries go with it:
        // nothing else can reach them once the id is gone, and the splitter
        // runs on every paragraph boundary of a streamed reply.
        if let (Some(caches), Some(MessageBlock::Text(block))) =
            (render_caches, blocks.get(tail_idx))
        {
            caches.evict_block(block.id);
            caches.evict_markdown(block.id);
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

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Text};
    use ratatui::widgets::{Paragraph, Wrap};
    use unicode_width::UnicodeWidthStr;

    use super::append_agent_stream_text;
    use crate::app::{ChatMessage, MessageBlock, MessageRole};
    use crate::ui::message::{
        MessageRenderContext, MessageRenderOptions, SpinnerState, render_message,
    };

    fn row_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    /// What one logical row costs once the frame paints it.
    ///
    /// `chat::render` wraps every row through `Paragraph::wrap`, so a row
    /// wider than the column is several drawn rows and `render_message`'s row
    /// count is not the drawn height. Same `line_count` the frame's own height
    /// arithmetic uses.
    fn drawn_rows(line: &Line<'static>, width: u16) -> usize {
        Paragraph::new(Text::from(vec![line.clone()]))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .max(1)
    }

    fn rows_are_bold(line: &Line<'static>) -> bool {
        let carries = |style: ratatui::style::Style| style.add_modifier.contains(Modifier::BOLD);
        carries(line.style) || line.spans.iter().any(|span| carries(span.style))
    }

    /// The terminal half of the density instrument.
    ///
    /// `#[ignore]`d on purpose: it asserts nothing and writes a file, and
    /// `scripts/density/measure.mjs` is the only caller. The fixture, the
    /// width and the output path arrive through the environment.
    ///
    /// It reports a band structure - each run of non-blank rows is one
    /// markdown block, each run of blank rows the gap between two - because
    /// "one blank row between blocks" is the terminal's entire spacing policy
    /// and this is the only place it can be read off.
    ///
    /// The message is built by `append_agent_stream_text`, which is what a
    /// live turn goes through: the reply arrives as one streamed chunk and the
    /// splitter cuts it at paragraph boundaries, each completed block carrying
    /// its own trailing break. Handing the text to a single `from_complete`
    /// block instead draws the paragraphs with no gap between them, which is
    /// not what the terminal shows.
    #[test]
    #[ignore = "measurement instrument; run by scripts/density/measure.mjs"]
    fn density_probe_writes_rows_json() {
        let fixture = std::env::var("FORGE_DENSITY_FIXTURE")
            .expect("FORGE_DENSITY_FIXTURE names the markdown to render");
        let out =
            std::env::var("FORGE_DENSITY_OUT").expect("FORGE_DENSITY_OUT names the file to write");
        let width: u16 = std::env::var("FORGE_DENSITY_WIDTH")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(100);
        let text = std::fs::read_to_string(&fixture).expect("the fixture reads");

        let mut blocks = Vec::new();
        append_agent_stream_text(&mut blocks, None, &text);
        let text_block_count =
            blocks.iter().filter(|block| matches!(block, MessageBlock::Text(..))).count();
        let mut message = ChatMessage::new(MessageRole::Assistant, blocks);

        let spinner = SpinnerState {
            glyph: '\u{280B}',
            is_active_turn_assistant: false,
            show_empty_thinking: false,
            show_thinking: false,
            show_compacting: false,
            live_turn_running: false,
        };
        let options = MessageRenderOptions {
            tools_collapsed: true,
            include_trailing_separator: false,
            ..MessageRenderOptions::default()
        };
        let mut lines = Vec::new();
        render_message(
            &mut message,
            &spinner,
            MessageRenderContext::new(None, width, 0, options),
            &mut lines,
        );

        let heights: Vec<usize> = lines.iter().map(|line| drawn_rows(line, width)).collect();
        let drawn_total: usize = heights.iter().sum();
        // Coverage assertion: the per-row decomposition has to add up to what
        // the frame measures for the whole block. If it does not, the band
        // figures rest on a wrap model the frame does not use, and every
        // number below is wrong in a way nothing else here would catch.
        let whole =
            Paragraph::new(Text::from(lines.clone())).wrap(Wrap { trim: false }).line_count(width);
        assert_eq!(drawn_total, whole, "per-row drawn heights must sum to the whole-block height");

        let mut bands = Vec::new();
        let mut gap = 0usize;
        let mut index = 0usize;
        while index < lines.len() {
            if row_text(&lines[index]).trim().is_empty() {
                gap += heights[index];
                index += 1;
                continue;
            }
            let start = index;
            while index < lines.len() && !row_text(&lines[index]).trim().is_empty() {
                index += 1;
            }
            let widest = (start..index)
                .map(|at| row_text(&lines[at]).width().min(usize::from(width)))
                .max()
                .unwrap_or(0);
            bands.push(serde_json::json!({
                "label": row_text(&lines[start]).trim().chars().take(40).collect::<String>(),
                "logical": index - start,
                "drawn": heights[start..index].iter().sum::<usize>(),
                "gap_before": std::mem::take(&mut gap),
                "bold": (start..index).any(|at| rows_are_bold(&lines[at])),
                "widest": widest,
            }));
        }

        let report = serde_json::json!({
            "width": width,
            "text_blocks": text_block_count,
            "logical_rows": lines.len(),
            "drawn_rows": whole,
            "trailing_gap": gap,
            "bands": bands,
            "rows": lines.iter().map(row_text).collect::<Vec<String>>(),
        });
        std::fs::write(&out, serde_json::to_string_pretty(&report).expect("the report serializes"))
            .expect("the report writes");
    }
}
