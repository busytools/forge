//! The view's render caches, held off the model.
//!
//! Entries are keyed by an identity - a tool call's id, a block's id, a
//! message's id - and each entry's stamp or key comparison carries what
//! decides staleness, so a wrong hit is impossible rather than merely
//! detected. No renderer knows its message index, so a positional key is
//! not available even if it were wanted.
//!
//! Reached through `MessageRenderContext`, which is `Copy` and re-borrowed
//! at every level of the render path, so the store mutates through a
//! `RefCell` rather than a `&mut` field.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::ops::Range;

use ratatui::text::Line;

use super::block_cache::BlockCache;
use forge_sessions::model::{BlockId, MessageId};

/// What a cached entry's lines were rendered for. Every input that changes
/// the rows is part of the stamp rather than the key, so a change replaces
/// the entry instead of accumulating one per version.
#[derive(PartialEq)]
enum SlotStamp {
    ToolCall {
        render_epoch: u64,
    },
    /// A text block's rows. The key is the block's id, so the stamp carries
    /// everything the id cannot see: the content fold, and the two view
    /// inputs that reach the rows through the renderer - the collapse flag
    /// and the role's newline and gutter treatment.
    Text {
        content_signature: u64,
        tools_collapsed: bool,
        preserve_newlines: bool,
        gutter: u16,
    },
}

struct SlotEntry {
    stamp: SlotStamp,
    cache: BlockCache,
}

/// A slot's cache for the duration of one caller. `Uncached` carries a
/// throwaway that renders identical lines and cannot store them, reached
/// when the store is already borrowed by an enclosing frame; it is what
/// keeps a re-entrant take from being a panic.
pub(crate) enum SlotCache<'a> {
    Stored(RefMut<'a, BlockCache>),
    Uncached(BlockCache),
}

/// As [`SlotCache`], for a caller that only reads.
pub(crate) enum SlotCacheRef<'a> {
    Stored(Ref<'a, BlockCache>),
    Uncached(BlockCache),
}

impl std::ops::Deref for SlotCache<'_> {
    type Target = BlockCache;

    fn deref(&self) -> &BlockCache {
        match self {
            Self::Stored(cache) => cache,
            Self::Uncached(cache) => cache,
        }
    }
}

impl std::ops::DerefMut for SlotCache<'_> {
    fn deref_mut(&mut self) -> &mut BlockCache {
        match self {
            Self::Stored(cache) => cache,
            Self::Uncached(cache) => cache,
        }
    }
}

impl std::ops::Deref for SlotCacheRef<'_> {
    type Target = BlockCache;

    fn deref(&self) -> &BlockCache {
        match self {
            Self::Stored(cache) => cache,
            Self::Uncached(cache) => cache,
        }
    }
}

/// Every render cache in the view. Every map here is keyed by an identity -
/// a tool call's id, a block's id, a message's id - with whatever changes
/// the rows carried in the entry's stamp or its key comparison. A content
/// key cannot work for a thing that streams: each chunk would get its own
/// entry, the old one would be unreachable, and the entry count would grow
/// with the frames rather than with the messages.
#[derive(Default)]
pub(crate) struct RenderCacheStore {
    tool_calls: RefCell<HashMap<Box<str>, SlotEntry>>,
    /// Each text block's rows, keyed by `TextBlock::id`.
    blocks: RefCell<HashMap<BlockId, SlotEntry>>,
    /// The welcome card, keyed by `hash_welcome_block_content`. Its own map
    /// rather than the blocks map because it folds its own content and no
    /// view input applies to it.
    welcome: RefCell<HashMap<u64, BlockCache>>,
    /// Each block's incremental markdown, keyed by `TextBlock::id`.
    markdown: RefCell<HashMap<BlockId, IncrementalMarkdown>>,
    /// Each message's layout, keyed by `ChatMessage::id`.
    messages: RefCell<HashMap<MessageId, MessageRenderCache>>,
}

impl RenderCacheStore {
    /// The cache for `id`, inserting a fresh empty one when the tool call
    /// has none or its entry was stored at a different render epoch.
    /// Replacing a stale entry is where an invalidation counts as applied.
    pub(crate) fn tool_call(&self, id: &str, render_epoch: u64) -> SlotCache<'_> {
        let Ok(mut entries) = self.tool_calls.try_borrow_mut() else {
            return SlotCache::Uncached(BlockCache::default());
        };
        if !entries.get(id).is_some_and(|entry| stored_at(entry, render_epoch)) {
            if entries.contains_key(id) {
                crate::perf::mark("tc_invalidations_applied");
            }
            entries.insert(id.into(), fresh_tool_call(render_epoch));
        }
        match RefMut::filter_map(entries, |map| map.get_mut(id)) {
            Ok(entry) => SlotCache::Stored(RefMut::map(entry, |entry| &mut entry.cache)),
            Err(_) => SlotCache::Uncached(BlockCache::default()),
        }
    }

    /// The cache for `id` when one is stored at this epoch, for a caller
    /// that only reads it - the render-budget accounting, which must not
    /// insert an entry just to measure one.
    pub(crate) fn peek_tool_call(&self, id: &str, render_epoch: u64) -> SlotCacheRef<'_> {
        let Ok(entries) = self.tool_calls.try_borrow() else {
            return SlotCacheRef::Uncached(BlockCache::default());
        };
        match Ref::filter_map(entries, |map| {
            map.get(id).filter(|entry| stored_at(entry, render_epoch)).map(|entry| &entry.cache)
        }) {
            Ok(cache) => SlotCacheRef::Stored(cache),
            Err(_) => SlotCacheRef::Uncached(BlockCache::default()),
        }
    }

    /// Drop `id`'s cached lines and report the bytes freed.
    pub(crate) fn evict_tool_call(&self, id: &str) -> usize {
        let Ok(mut entries) = self.tool_calls.try_borrow_mut() else {
            return 0;
        };
        entries.get_mut(id).map_or(0, |entry| entry.cache.evict_cached_render())
    }

    /// The cache for the block with `block_id`, inserting a fresh empty one
    /// when there is none or when the stored rows were built from different
    /// inputs.
    pub(crate) fn text_block(
        &self,
        block_id: BlockId,
        content_signature: u64,
        tools_collapsed: bool,
        preserve_newlines: bool,
        gutter: u16,
    ) -> SlotCache<'_> {
        let stamp =
            SlotStamp::Text { content_signature, tools_collapsed, preserve_newlines, gutter };
        let Ok(mut entries) = self.blocks.try_borrow_mut() else {
            return SlotCache::Uncached(BlockCache::default());
        };
        if !entries.get(&block_id).is_some_and(|entry| entry.stamp == stamp) {
            entries.insert(block_id, SlotEntry { stamp, cache: BlockCache::default() });
        }
        match RefMut::filter_map(entries, |map| map.get_mut(&block_id)) {
            Ok(entry) => SlotCache::Stored(RefMut::map(entry, |entry| &mut entry.cache)),
            Err(_) => SlotCache::Uncached(BlockCache::default()),
        }
    }

    /// The rows stored for `block_id`, for a caller that only reads them.
    /// The render-budget walk holds no view inputs, so this matches on the
    /// id alone - the entry under a block's id IS that block's cache.
    pub(crate) fn peek_block(&self, block_id: BlockId) -> SlotCacheRef<'_> {
        let Ok(entries) = self.blocks.try_borrow() else {
            return SlotCacheRef::Uncached(BlockCache::default());
        };
        match Ref::filter_map(entries, |map| map.get(&block_id).map(|entry| &entry.cache)) {
            Ok(cache) => SlotCacheRef::Stored(cache),
            Err(_) => SlotCacheRef::Uncached(BlockCache::default()),
        }
    }

    /// Drop `block_id`'s entry and report the bytes freed.
    ///
    /// The entry is removed rather than emptied: its id is the only handle
    /// it has, so an emptied entry could never be reached again, and a
    /// block that leaves the model would leave its rows behind.
    pub(crate) fn evict_block(&self, block_id: BlockId) -> usize {
        let Ok(mut entries) = self.blocks.try_borrow_mut() else {
            return 0;
        };
        entries.remove(&block_id).map_or(0, |mut entry| entry.cache.evict_cached_render())
    }

    /// The welcome card's cache, keyed by what it renders to.
    pub(crate) fn welcome(&self, content_signature: u64) -> SlotCache<'_> {
        let Ok(mut entries) = self.welcome.try_borrow_mut() else {
            return SlotCache::Uncached(BlockCache::default());
        };
        entries.entry(content_signature).or_default();
        match RefMut::filter_map(entries, |map| map.get_mut(&content_signature)) {
            Ok(cache) => SlotCache::Stored(cache),
            Err(_) => SlotCache::Uncached(BlockCache::default()),
        }
    }

    /// The welcome card's cache, for a caller that only reads it.
    pub(crate) fn peek_welcome(&self, content_signature: u64) -> SlotCacheRef<'_> {
        let Ok(entries) = self.welcome.try_borrow() else {
            return SlotCacheRef::Uncached(BlockCache::default());
        };
        match Ref::filter_map(entries, |map| map.get(&content_signature)) {
            Ok(cache) => SlotCacheRef::Stored(cache),
            Err(_) => SlotCacheRef::Uncached(BlockCache::default()),
        }
    }

    /// Drop the welcome card's cache and report the bytes freed.
    pub(crate) fn evict_welcome(&self, content_signature: u64) -> usize {
        let Ok(mut entries) = self.welcome.try_borrow_mut() else {
            return 0;
        };
        entries.remove(&content_signature).map_or(0, |mut cache| cache.evict_cached_render())
    }

    /// The incremental markdown for the block with `id`, rebuilt from `text`
    /// when the stored entry's text is not a prefix of it.
    ///
    /// **The id is the key because a streaming block's content changes on
    /// every append.** A content key would give each intermediate state its
    /// own entry, so the rendered chunks would never be reused and the
    /// incremental render - the whole point of this cache - would stop
    /// working. The prefix test is the validity check, and it is what
    /// catches a block whose text was REPLACED in place: same id, not a
    /// prefix. A reused entry keeps its chunks, so an append re-renders only
    /// the tail.
    pub(crate) fn markdown(&self, id: BlockId, text: &str) -> SlotMarkdown<'_> {
        let Ok(mut entries) = self.markdown.try_borrow_mut() else {
            return SlotMarkdown::Uncached(IncrementalMarkdown::from_complete(text));
        };
        if entries.get(&id).is_none_or(|entry| !entry.covers(text)) {
            entries.insert(id, IncrementalMarkdown::from_complete(text));
        }
        match RefMut::filter_map(entries, |map| map.get_mut(&id)) {
            Ok(entry) => SlotMarkdown::Stored(entry),
            Err(_) => SlotMarkdown::Uncached(IncrementalMarkdown::from_complete(text)),
        }
    }

    /// The incremental markdown for `id`, for a caller that only reads it -
    /// the history-retention accounting, which measures its capacity.
    pub(crate) fn peek_markdown(&self, id: BlockId) -> Option<Ref<'_, IncrementalMarkdown>> {
        let entries = self.markdown.try_borrow().ok()?;
        Ref::filter_map(entries, |map| map.get(&id)).ok()
    }

    /// Drop `id`'s incremental markdown and report the bytes it held.
    pub(crate) fn evict_markdown(&self, id: BlockId) -> usize {
        let Ok(mut entries) = self.markdown.try_borrow_mut() else {
            return 0;
        };
        entries.remove(&id).map_or(0, |entry| entry.text_capacity())
    }
}

/// A block's incremental markdown for the duration of one caller.
pub(crate) enum SlotMarkdown<'a> {
    Stored(RefMut<'a, IncrementalMarkdown>),
    Uncached(IncrementalMarkdown),
}

impl std::ops::Deref for SlotMarkdown<'_> {
    type Target = IncrementalMarkdown;

    fn deref(&self) -> &IncrementalMarkdown {
        match self {
            Self::Stored(markdown) => markdown,
            Self::Uncached(markdown) => markdown,
        }
    }
}

impl std::ops::DerefMut for SlotMarkdown<'_> {
    fn deref_mut(&mut self) -> &mut IncrementalMarkdown {
        match self {
            Self::Stored(markdown) => markdown,
            Self::Uncached(markdown) => markdown,
        }
    }
}

/// What a markdown chunk was rendered for.
///
/// The render INPUTS, not an identity: this is the stamp that invalidates a
/// chunk on a width or gutter change, and it can never be a cache key - two
/// blocks at one width would share an entry whose chunks are byte ranges
/// into a different block's text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarkdownRenderKey {
    pub width: u16,
    /// Blank columns the caller reserves at the left of every emitted row
    /// for the user turn's gutter. Zero for every other role.
    pub gutter: u16,
    pub preserve_newlines: bool,
}

struct MarkdownChunk {
    range: Range<usize>,
    rendered: Option<RenderedChunk>,
    render_key: Option<MarkdownRenderKey>,
    dirty: bool,
}

/// One chunk's render: the lines plus the copy provenance the row builders
/// produced alongside them.
pub(crate) struct RenderedChunk {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) copy_rows: Vec<crate::ui::copy::CopyRowMeta>,
}

impl MarkdownChunk {
    fn new(range: Range<usize>) -> Self {
        Self { range, rendered: None, render_key: None, dirty: true }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::{IncrementalMarkdown, MarkdownRenderKey, RenderedChunk};
    use pretty_assertions::assert_eq;

    /// Simple render function for tests: wraps each line in a `Line`.
    fn test_render(src: &str) -> RenderedChunk {
        RenderedChunk {
            lines: src.lines().map(|l| Line::from(l.to_owned())).collect(),
            copy_rows: Vec::new(),
        }
    }

    fn test_render_key() -> MarkdownRenderKey {
        MarkdownRenderKey { width: 80, gutter: 0, preserve_newlines: false }
    }

    #[test]
    fn incr_default_empty() {
        let incr = IncrementalMarkdown::default();
        assert!(incr.full_text().is_empty());
    }

    #[test]
    fn incr_from_complete() {
        let incr = IncrementalMarkdown::from_complete("hello world");
        assert_eq!(incr.full_text(), "hello world");
    }

    #[test]
    fn incr_append_single_chunk() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("hello");
        assert_eq!(incr.full_text(), "hello");
    }

    #[test]
    fn incr_append_accumulates_chunks() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("line1");
        incr.append("\nline2");
        incr.append("\nline3");
        assert_eq!(incr.full_text(), "line1\nline2\nline3");
    }

    #[test]
    fn incr_append_preserves_paragraph_delimiters() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("para1\n\npara2");
        assert_eq!(incr.full_text(), "para1\n\npara2");
    }

    #[test]
    fn incr_full_text_reconstruction() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("p1\n\np2\n\np3");
        assert_eq!(incr.full_text(), "p1\n\np2\n\np3");
    }

    #[test]
    fn incr_lines_renders_all() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("line1\n\nline2\n\nline3");
        let lines = incr.lines(test_render_key(), &test_render);
        // test_render maps each source line to one output line
        assert_eq!(lines.lines.len(), 5);
    }

    #[test]
    fn incr_ensure_rendered_preserves_text() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("p1\n\np2\n\ntail");
        incr.ensure_rendered(test_render_key(), &test_render);
        assert_eq!(incr.full_text(), "p1\n\np2\n\ntail");
    }

    /// This test is the ONLY guard the markdown cache has: nothing else
    /// re-derives the markdown from the block, and every render re-validates
    /// through the prefix test before reusing an entry. Soften it and a
    /// future `block.text = old + "..."` that skips the store renders the
    /// truncated old text silently - the store would double-buffer the
    /// append instead, which is why nothing else catches it.
    #[test]
    fn incr_reuses_rendered_prefix_chunks() {
        use std::cell::Cell;

        let calls = Cell::new(0usize);
        let render = |src: &str| -> RenderedChunk {
            calls.set(calls.get() + 1);
            test_render(src)
        };

        let mut incr = IncrementalMarkdown::default();
        incr.append("p1\n\np2");
        let _ = incr.lines(test_render_key(), &render);
        assert_eq!(calls.get(), 2);

        incr.append(" tail");
        let _ = incr.lines(test_render_key(), &render);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn incr_does_not_split_inside_fenced_code_blocks() {
        let sources = std::cell::RefCell::new(Vec::new());
        let render = |src: &str| -> RenderedChunk {
            sources.borrow_mut().push(src.to_owned());
            test_render(src)
        };

        let mut incr = IncrementalMarkdown::default();
        incr.append("```rust\nfn main() {\n\nprintln!(\"hi\");\n}\n```\n\nafter");
        let _ = incr.lines(test_render_key(), &render);

        assert_eq!(
            sources.borrow().as_slice(),
            ["```rust\nfn main() {\n\nprintln!(\"hi\");\n}\n```\n\n", "after"],
            "the split lands on the blank line after the fence, not the one inside it"
        );
    }

    #[test]
    fn incr_does_not_split_inside_a_four_backtick_fence() {
        let sources = std::cell::RefCell::new(Vec::new());
        let render = |src: &str| -> RenderedChunk {
            sources.borrow_mut().push(src.to_owned());
            test_render(src)
        };

        let mut incr = IncrementalMarkdown::default();
        incr.append("````\n```\nfirst\n\nsecond\n```\n````\n\nafter");
        let _ = incr.lines(test_render_key(), &render);

        assert_eq!(
            sources.borrow().as_slice(),
            ["````\n```\nfirst\n\nsecond\n```\n````\n\n", "after"],
            "the inner fence is content, so the blank line inside stays with it"
        );
    }

    #[test]
    fn incr_streaming_simulation() {
        // Simulate a realistic streaming scenario
        let mut incr = IncrementalMarkdown::default();
        let chunks = ["Here is ", "some text.\n", "\nNext para", "graph here.\n\n", "Final."];
        for chunk in chunks {
            incr.append(chunk);
        }
        assert_eq!(incr.full_text(), "Here is some text.\n\nNext paragraph here.\n\nFinal.");
    }
}

/// A block's markdown source, with its stable paragraph-sized prefixes
/// rendered once so only the active tail re-renders while text streams in.
#[derive(Default)]
pub struct IncrementalMarkdown {
    text: String,
    chunks: Vec<MarkdownChunk>,
}

impl IncrementalMarkdown {
    /// Create from existing full text (e.g. user messages, connection errors).
    /// Treats the entire text as one block source.
    pub fn from_complete(text: &str) -> Self {
        let mut markdown = Self::default();
        markdown.append(text);
        markdown
    }

    /// True when this cache's text is a prefix of `text`, which is what makes
    /// its rendered chunks usable for `text`.
    pub(crate) fn covers(&self, text: &str) -> bool {
        text.starts_with(self.full_text())
    }

    /// Append a streaming text chunk.
    pub fn append(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.text.push_str(chunk);
        if let Some(last) = self.chunks.last_mut() {
            last.range.end = self.text.len();
            last.dirty = true;
            last.rendered = None;
            last.render_key = None;
        } else {
            self.chunks.push(MarkdownChunk::new(0..self.text.len()));
        }
        self.split_tail_chunks();
    }

    /// The source text this cache was built from.
    pub fn full_text(&self) -> &str {
        &self.text
    }

    /// Allocated capacity of the internal text buffer in bytes.
    pub fn text_capacity(&self) -> usize {
        self.text.capacity()
    }

    /// Render this block source via the provided markdown renderer.
    /// `render_fn` converts a markdown source string into `Vec<Line>`.
    pub(crate) fn lines(
        &mut self,
        render_key: MarkdownRenderKey,
        render_fn: &impl Fn(&str) -> RenderedChunk,
    ) -> RenderedChunk {
        self.ensure_rendered(render_key, render_fn);

        let mut rendered = RenderedChunk { lines: Vec::new(), copy_rows: Vec::new() };
        for chunk in &self.chunks {
            if let Some(chunk_rendered) = &chunk.rendered {
                rendered.lines.extend(chunk_rendered.lines.iter().cloned());
                rendered.copy_rows.extend(chunk_rendered.copy_rows.iter().cloned());
            }
        }
        rendered
    }

    pub(crate) fn ensure_rendered(
        &mut self,
        render_key: MarkdownRenderKey,
        render_fn: &impl Fn(&str) -> RenderedChunk,
    ) {
        for idx in 0..self.chunks.len() {
            let needs_render = {
                let chunk = &self.chunks[idx];
                chunk.dirty || chunk.rendered.is_none() || chunk.render_key != Some(render_key)
            };
            if !needs_render {
                continue;
            }

            let range = self.chunks[idx].range.clone();
            let rendered = render_fn(&self.text[range]);
            let chunk = &mut self.chunks[idx];
            chunk.rendered = Some(rendered);
            chunk.render_key = Some(render_key);
            chunk.dirty = false;
        }
    }

    /// Split the tail into stable paragraph-sized chunks. `while let` rather
    /// than `loop` so the empty-chunks case is the loop's own exit rather
    /// than an allow on the lint.
    fn split_tail_chunks(&mut self) {
        while let Some(last_idx) = self.chunks.len().checked_sub(1) {
            let range = self.chunks[last_idx].range.clone();
            let Some(split_at_rel) = find_first_stable_split(&self.text[range.clone()]) else {
                break;
            };
            let split_at = range.start + split_at_rel;
            if split_at <= range.start || split_at >= range.end {
                break;
            }

            self.chunks[last_idx] = MarkdownChunk::new(range.start..split_at);
            self.chunks.push(MarkdownChunk::new(split_at..range.end));
        }
    }
}

fn find_first_stable_split(text: &str) -> Option<usize> {
    let code = crate::ui::fence::code_ranges(text);
    let mut saw_nonblank = false;
    let mut blank_run_end = None;
    let mut offset = 0usize;
    // The ranges are in source order, so one cursor walks them with the
    // line offsets instead of re-checking every range per line.
    let mut next_range = 0usize;

    for line in text.split_inclusive('\n') {
        while code.get(next_range).is_some_and(|range| range.end <= offset) {
            next_range += 1;
        }
        let in_fenced_code = code.get(next_range).is_some_and(|range| range.contains(&offset));
        offset += line.len();
        let trimmed = line.trim_end_matches('\n').trim();

        let is_blank = trimmed.is_empty();
        if !in_fenced_code && is_blank {
            if saw_nonblank {
                blank_run_end = Some(offset);
            }
            continue;
        }

        if let Some(boundary) = blank_run_end.take()
            && boundary < text.len()
        {
            return Some(boundary);
        }

        if !is_blank {
            saw_nonblank = true;
        }
    }

    None
}

fn stored_at(entry: &SlotEntry, render_epoch: u64) -> bool {
    matches!(entry.stamp, SlotStamp::ToolCall { render_epoch: stored } if stored == render_epoch)
}

/// Cross-crate test access to the store.
///
/// The integration tests live in a separate crate, so seeding a cache and
/// reading its bytes cannot go through the crate-internal surface. Gated
/// like the other test helpers, so the install build's public API does not
/// carry a peek that only a test wanted.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use ratatui::text::Line;

    use crate::app::{App, TextBlock};
    use forge_sessions::model::BlockId;

    /// Store rendered rows for a text block, as the assistant renderer would.
    pub fn store_block(app: &App, block: &TextBlock, lines: Vec<Line<'static>>) {
        app.render_caches
            .text_block(block.id, block.content_signature(), false, false, 0)
            .store(lines);
    }

    /// The bytes held for a text block.
    pub fn block_bytes(app: &App, block: &TextBlock) -> usize {
        app.render_caches.peek_block(block.id).cached_bytes()
    }

    /// How many block entries the store holds - the leak check, since a
    /// content-keyed store grows one per version of a block.
    pub fn block_entry_count(app: &App) -> usize {
        app.render_caches.blocks.try_borrow().map_or(0, |entries| entries.len())
    }

    /// How many message entries the store holds, for the same check.
    pub fn message_entry_count(app: &App) -> usize {
        app.render_caches.messages.try_borrow().map_or(0, |entries| entries.len())
    }

    /// How many markdown entries the store holds. The splitter replaces a
    /// streamed block on every paragraph boundary, so this is where its
    /// orphans would show.
    pub fn markdown_entry_count(app: &App) -> usize {
        app.render_caches.markdown.try_borrow().map_or(0, |entries| entries.len())
    }

    /// The ids the store holds markdown for. An id the model no longer has
    /// is an orphan: nothing can reach the entry again.
    pub fn markdown_entry_ids(app: &App) -> Vec<BlockId> {
        app.render_caches
            .markdown
            .try_borrow()
            .map_or_else(|_| Vec::new(), |entries| entries.keys().copied().collect())
    }

    /// The retained-bytes estimate for one message, which sums a block's
    /// rows and its incremental markdown together.
    pub fn retained_bytes_estimate(app: &App, msg: &crate::app::ChatMessage) -> usize {
        App::measure_message_bytes(&app.render_caches, msg)
    }
}

fn fresh_tool_call(render_epoch: u64) -> SlotEntry {
    SlotEntry { stamp: SlotStamp::ToolCall { render_epoch }, cache: BlockCache::default() }
}

/// The render inputs a cached message layout was built from.
///
/// Distinct from `MessageRenderSignature`, which is the model's
/// content-only fold: this adds the view's own inputs, so an entry can
/// tell whether the layout it holds was built for the render about to
/// paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRenderCacheKey {
    pub width: u16,
    pub layout_generation: u64,
    pub tools_collapsed: bool,
    pub include_trailing_separator: bool,
    /// #273: Action count from the `Message::StopHookSummary` bound
    /// to this message (`0` when no summary applies). Folded into the
    /// cache key so a fresh summary event reliably invalidates the
    /// prior render even when the underlying assistant blocks didn't
    /// change.
    pub stop_hook_summary_actions: u32,
    /// #273: Toggle for the stop-hook-summary expanded body. Folded
    /// into the cache key so click-to-expand flips re-render without
    /// extra coordination.
    pub stop_hook_summary_expanded: bool,
    pub render_signature: super::messages::MessageRenderSignature,
}

#[derive(Default)]
pub struct MessageRenderCache {
    key: Option<MessageRenderCacheKey>,
    segments: Vec<CachedMessageSegment>,
    cached_bytes: usize,
    height: usize,
    wrapped_lines: usize,
    /// Wrapped-row ranges, from the message's first row, that the
    /// user-turn gutter covers. Empty for every other role.
    gutter_rows: Vec<Range<usize>>,
    /// Copy provenance, one entry per rendered row, from the message's
    /// first row. Parallel to the flattened segment rows.
    copy_rows: Vec<crate::ui::copy::CopyRowMeta>,
    last_access_tick: Cell<u64>,
}

#[derive(Clone)]
pub enum CachedMessageSegment {
    Blank,
    Lines { lines: Vec<Line<'static>>, height: usize },
}

impl MessageRenderCache {
    fn touch(&self) {
        self.last_access_tick.set(super::block_cache::next_cache_access_tick());
    }

    pub fn matches(&self, key: &MessageRenderCacheKey) -> bool {
        self.key.as_ref() == Some(key)
    }

    pub fn segments(&self) -> &[CachedMessageSegment] {
        self.touch();
        &self.segments
    }

    pub fn height(&self) -> usize {
        self.touch();
        self.height
    }

    pub fn wrapped_lines(&self) -> usize {
        self.touch();
        self.wrapped_lines
    }

    pub fn gutter_rows(&self) -> &[Range<usize>] {
        self.touch();
        &self.gutter_rows
    }

    pub(crate) fn copy_rows(&self) -> &[crate::ui::copy::CopyRowMeta] {
        self.touch();
        &self.copy_rows
    }

    pub fn cached_bytes(&self) -> usize {
        self.cached_bytes
    }

    pub fn last_access_tick(&self) -> u64 {
        self.last_access_tick.get()
    }

    pub(crate) fn store(
        &mut self,
        key: MessageRenderCacheKey,
        segments: Vec<CachedMessageSegment>,
        height: usize,
        wrapped_lines: usize,
        gutter_rows: Vec<Range<usize>>,
        copy_rows: Vec<crate::ui::copy::CopyRowMeta>,
    ) {
        let cached_bytes = segments.iter().map(CachedMessageSegment::cached_bytes).sum();
        self.key = Some(key);
        self.segments = segments;
        self.cached_bytes = cached_bytes;
        self.height = height;
        self.wrapped_lines = wrapped_lines;
        self.gutter_rows = gutter_rows;
        self.copy_rows = copy_rows;
        self.touch();
    }

    pub fn invalidate(&mut self) {
        self.key = None;
        self.segments.clear();
        self.cached_bytes = 0;
        self.height = 0;
        self.wrapped_lines = 0;
        self.gutter_rows.clear();
        self.copy_rows.clear();
    }

    pub fn evict_cached_render(&mut self) -> usize {
        let removed = self.cached_bytes;
        if removed == 0 {
            return 0;
        }
        self.invalidate();
        removed
    }
}

impl CachedMessageSegment {
    fn cached_bytes(&self) -> usize {
        match self {
            Self::Blank => 1,
            Self::Lines { lines, .. } => lines.iter().map(line_utf8_bytes).sum(),
        }
    }
}

fn line_utf8_bytes(line: &Line<'static>) -> usize {
    let span_bytes =
        line.spans.iter().fold(0usize, |acc, span| acc.saturating_add(span.content.len()));
    span_bytes.saturating_add(1)
}

/// A message's cached layout for the duration of one caller, shaped like
/// [`SlotCache`]: `Uncached` renders identical rows and cannot store them.
pub enum MessageSlot<'a> {
    Stored(RefMut<'a, MessageRenderCache>),
    Uncached(MessageRenderCache),
}

impl std::ops::Deref for MessageSlot<'_> {
    type Target = MessageRenderCache;

    fn deref(&self) -> &MessageRenderCache {
        match self {
            Self::Stored(cache) => cache,
            Self::Uncached(cache) => cache,
        }
    }
}

impl std::ops::DerefMut for MessageSlot<'_> {
    fn deref_mut(&mut self) -> &mut MessageRenderCache {
        match self {
            Self::Stored(cache) => cache,
            Self::Uncached(cache) => cache,
        }
    }
}

impl RenderCacheStore {
    /// The layout for the message with `message_id`, inserting an empty one
    /// when there is none. The caller compares
    /// [`MessageRenderCache::matches`] against its full key and rebuilds
    /// when it differs, which is what keeps the entry honest as the message
    /// changes - the id is the key, the content is the invalidation.
    pub(crate) fn message(&self, message_id: MessageId) -> MessageSlot<'_> {
        let Ok(mut entries) = self.messages.try_borrow_mut() else {
            return MessageSlot::Uncached(MessageRenderCache::default());
        };
        entries.entry(message_id).or_default();
        match RefMut::filter_map(entries, |map| map.get_mut(&message_id)) {
            Ok(entry) => MessageSlot::Stored(entry),
            Err(_) => MessageSlot::Uncached(MessageRenderCache::default()),
        }
    }

    /// The layout stored for `message_id`, for a caller that only reads it -
    /// the render-budget accounting, which must not insert one just to
    /// measure.
    pub(crate) fn peek_message(&self, message_id: MessageId) -> MessageSlotRef<'_> {
        let Ok(entries) = self.messages.try_borrow() else {
            return MessageSlotRef::Uncached(MessageRenderCache::default());
        };
        match Ref::filter_map(entries, |map| map.get(&message_id)) {
            Ok(entry) => MessageSlotRef::Stored(entry),
            Err(_) => MessageSlotRef::Uncached(MessageRenderCache::default()),
        }
    }

    /// Drop `message_id`'s entry and report the bytes freed. Removed rather
    /// than emptied, for [`RenderCacheStore::evict_block`]'s reason.
    pub(crate) fn evict_message(&self, message_id: MessageId) -> usize {
        let Ok(mut entries) = self.messages.try_borrow_mut() else {
            return 0;
        };
        entries.remove(&message_id).map_or(0, |mut cache| cache.evict_cached_render())
    }
}

/// As [`MessageSlot`], for a caller that only reads.
pub enum MessageSlotRef<'a> {
    Stored(Ref<'a, MessageRenderCache>),
    Uncached(MessageRenderCache),
}

impl std::ops::Deref for MessageSlotRef<'_> {
    type Target = MessageRenderCache;

    fn deref(&self) -> &MessageRenderCache {
        match self {
            Self::Stored(cache) => cache,
            Self::Uncached(cache) => cache,
        }
    }
}
