use super::block_cache::BlockCache;
use super::tool_call_info::ToolCallInfo;
use super::types::MessageUsage;
use ratatui::style::Color;
use ratatui::text::Line;
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ChatMessage {
    pub role: MessageRole,
    pub blocks: Vec<MessageBlock>,
    pub usage: Option<MessageUsage>,
    pub render_cache: MessageRenderCache,
    /// #143 item 2: cached peer-envelope flag stamped once at push
    /// time (the `PeerEnvelopeAppended` reducer + similar entry
    /// points) so the chat renderer doesn't walk text blocks every
    /// frame to recompute it. The flag is intrinsic to the message's
    /// FIRST text block: a peer envelope arrives as a User-role
    /// message whose first text block starts with a recognised
    /// bracket prefix (`[Question id=...]` / `[Message id=...]` / etc.).
    pub is_peer_envelope: bool,
    /// #273: stop_hook_summary chip hit-test - wrapped-row offset
    /// inside this message of the clickable chip line(s). `0` when
    /// no chip is rendered. Stamped by `append_stop_hook_summary`.
    pub stop_hook_summary_y_in_msg: usize,
    /// #273: stop_hook_summary chip hit-test - wrapped-row height of
    /// the clickable chip line(s) (excludes the leading blank and
    /// any expanded hook rows). `0` when no chip is rendered.
    pub stop_hook_summary_height: usize,
    /// #275 Bug 2: per-message turn-duration captured from the
    /// `Message::TurnDuration` event that fires at end-of-turn. The
    /// `handle_turn_duration` reducer walks back to find the most
    /// recent Assistant message and stamps this field; the role-label
    /// renderer surfaces it as the `Forge - 12.4s` banner chip. Stays
    /// on the message itself (not the spinner) so the chip persists
    /// across scrollback - the prior spinner-state approach gated the
    /// chip on the now-stale `is_active_turn_assistant` flag and
    /// dropped immediately as soon as the turn became "past".
    pub turn_duration_ms: Option<u64>,
}

impl ChatMessage {
    pub fn new(role: MessageRole, blocks: Vec<MessageBlock>, usage: Option<MessageUsage>) -> Self {
        Self {
            role,
            blocks,
            usage,
            render_cache: MessageRenderCache::default(),
            is_peer_envelope: false,
            stop_hook_summary_y_in_msg: 0,
            stop_hook_summary_height: 0,
            turn_duration_ms: None,
        }
    }

    /// Variant of `new` for envelope messages that pre-stamps the
    /// peer-envelope flag. Used by the `PeerEnvelopeAppended`
    /// reducer + any other site that constructs a known-envelope
    /// `ChatMessage`. Avoids the render-time `detect_inbound` walk
    /// over the text blocks.
    pub fn new_peer_envelope(
        role: MessageRole,
        blocks: Vec<MessageBlock>,
        usage: Option<MessageUsage>,
    ) -> Self {
        Self {
            role,
            blocks,
            usage,
            render_cache: MessageRenderCache::default(),
            is_peer_envelope: true,
            stop_hook_summary_y_in_msg: 0,
            stop_hook_summary_height: 0,
            turn_duration_ms: None,
        }
    }

    pub fn welcome(version: &str, subscription: &str, cwd: &str, session_id: &str) -> Self {
        Self::new(
            MessageRole::Welcome,
            vec![MessageBlock::Welcome(WelcomeBlock {
                version: version.to_owned(),
                account_label: "Subscription".to_owned(),
                subscription: subscription.to_owned(),
                cwd: cwd.to_owned(),
                session_id: session_id.to_owned(),
                tip_seed: random_welcome_tip_seed(),
                cache: BlockCache::default(),
            })],
            None,
        )
    }

    pub fn invalidate_render_cache(&mut self) {
        self.render_cache.invalidate();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRenderCacheKey {
    pub width: u16,
    pub layout_generation: u64,
    pub tools_collapsed: bool,
    pub include_trailing_separator: bool,
    pub suppress_group_header: bool,
    /// Mirror of `MessageRenderOptions.envelope_streak_position`
    /// serialized as a small ordinal so the cache key stays
    /// `derive(PartialEq, Eq)`. `0` = None, `1` = Start, `2` =
    /// FollowerNewWorker, `3` = FollowerSameWorker.
    pub envelope_streak_position_ord: u8,
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
    pub render_signature: MessageRenderSignature,
}

/// Compact cache-key proxy for a [`ChatMessage`] + render context.
///
/// All inputs that affect the rendered output are folded into a single
/// `u64` hash: message role, spinner-state flags, the assistant frame
/// (when frame-dependent), and per-block contributions (text hashes,
/// tool-call epochs / status / permission flags, welcome-block hash,
/// image-attachment count). The render cache compares signatures by
/// `==`, which on a `u64` is one machine-word compare.
///
/// Hash collisions are theoretically possible (we trust 64-bit
/// `DefaultHasher`); a collision would manifest as a stale render
/// surviving past an input change. Acceptable for a render cache -
/// the next genuine state change invalidates everything anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageRenderSignature(pub u64);

#[derive(Default)]
pub struct MessageRenderCache {
    key: Option<MessageRenderCacheKey>,
    segments: Vec<CachedMessageSegment>,
    cached_bytes: usize,
    height: usize,
    wrapped_lines: usize,
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

    pub fn cached_bytes(&self) -> usize {
        self.cached_bytes
    }

    pub fn last_access_tick(&self) -> u64 {
        self.last_access_tick.get()
    }

    pub fn store(
        &mut self,
        key: MessageRenderCacheKey,
        segments: Vec<CachedMessageSegment>,
        height: usize,
        wrapped_lines: usize,
    ) {
        let cached_bytes = segments.iter().map(CachedMessageSegment::cached_bytes).sum();
        self.key = Some(key);
        self.segments = segments;
        self.cached_bytes = cached_bytes;
        self.height = height;
        self.wrapped_lines = wrapped_lines;
        self.touch();
    }

    pub fn invalidate(&mut self) {
        self.key = None;
        self.segments.clear();
        self.cached_bytes = 0;
        self.height = 0;
        self.wrapped_lines = 0;
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

pub fn hash_text_block_content(text: &str, trailing_spacing: TextBlockSpacing) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    trailing_spacing.hash(&mut hasher);
    hasher.finish()
}

pub fn hash_welcome_block_content(block: &WelcomeBlock) -> u64 {
    let mut hasher = DefaultHasher::new();
    block.version.hash(&mut hasher);
    block.account_label.hash(&mut hasher);
    block.subscription.hash(&mut hasher);
    block.cwd.hash(&mut hasher);
    block.session_id.hash(&mut hasher);
    block.tip_seed.hash(&mut hasher);
    hasher.finish()
}

fn random_welcome_tip_seed() -> u64 {
    let mut hasher = DefaultHasher::new();
    SystemTime::now().duration_since(UNIX_EPOCH).ok().hash(&mut hasher);
    hasher.finish()
}

fn line_utf8_bytes(line: &Line<'static>) -> usize {
    let span_bytes =
        line.spans.iter().fold(0usize, |acc, span| acc.saturating_add(span.content.len()));
    span_bytes.saturating_add(1)
}

/// Text holder for a single message block's markdown source.
///
/// Block splitting for streaming text is handled at the message construction
/// level. Within a block, this type keeps stable paragraph-sized prefixes cached
/// so only the active tail needs to be re-rendered while streaming continues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarkdownRenderKey {
    pub width: u16,
    pub bg: Option<Color>,
    pub preserve_newlines: bool,
}

#[derive(Default)]
struct MarkdownChunk {
    range: Range<usize>,
    rendered: Option<Vec<Line<'static>>>,
    render_key: Option<MarkdownRenderKey>,
    dirty: bool,
}

impl MarkdownChunk {
    fn new(range: Range<usize>) -> Self {
        Self { range, rendered: None, render_key: None, dirty: true }
    }
}

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

    /// Get the full source text.
    pub fn full_text(&self) -> String {
        self.text.clone()
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
        render_fn: &impl Fn(&str) -> Vec<Line<'static>>,
    ) -> Vec<Line<'static>> {
        self.ensure_rendered(render_key, render_fn);

        let mut rendered = Vec::new();
        for chunk in &self.chunks {
            if let Some(lines) = &chunk.rendered {
                rendered.extend(lines.iter().cloned());
            }
        }
        rendered
    }

    pub fn invalidate_renders(&mut self) {
        for chunk in &mut self.chunks {
            chunk.dirty = true;
            chunk.rendered = None;
            chunk.render_key = None;
        }
    }

    pub(crate) fn ensure_rendered(
        &mut self,
        render_key: MarkdownRenderKey,
        render_fn: &impl Fn(&str) -> Vec<Line<'static>>,
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

    fn split_tail_chunks(&mut self) {
        #[allow(clippy::while_let_loop)] // multiple early-break conditions inside
        loop {
            let Some(last_idx) = self.chunks.len().checked_sub(1) else {
                break;
            };
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
    let mut in_fenced_code = false;
    let mut saw_nonblank = false;
    let mut blank_run_end = None;
    let mut offset = 0usize;

    for line in text.split_inclusive('\n') {
        offset += line.len();
        let trimmed = line.trim_end_matches('\n').trim();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if is_fence {
            in_fenced_code = !in_fenced_code;
        }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TextBlockSpacing {
    #[default]
    None,
    ParagraphBreak,
}

impl TextBlockSpacing {
    pub fn blank_lines(self) -> usize {
        match self {
            Self::None => 0,
            Self::ParagraphBreak => 1,
        }
    }
}

pub struct TextBlock {
    pub text: String,
    pub cache: BlockCache,
    pub markdown: IncrementalMarkdown,
    /// Explicit visual spacing after this block.
    ///
    /// This is used when streaming splits one logical assistant message into
    /// multiple cached blocks at paragraph boundaries. Rendering consumes this
    /// metadata directly so spacing, height measurement, and scroll skipping all
    /// agree without mutating source text.
    pub trailing_spacing: TextBlockSpacing,
    /// Peer-coordination collapse override (#114). `Some(true)` /
    /// `Some(false)` pins this block's peer-envelope collapse state
    /// regardless of the global `app.tools_collapsed`. `None` ⇒
    /// follow the global default. Set by the mouse click handler
    /// when the user toggles an inbound peer row. Always `None` for
    /// non-peer text blocks.
    pub peer_collapsed_override: Option<bool>,
    /// Row offset within the rendered message at which the peer
    /// block starts (post-layout). Stamped each frame by the user-
    /// block renderer when a peer envelope is detected. Used by
    /// `mouse::locate_peer_user_block_at_click` to find the clicked
    /// block. Zero for non-peer text blocks.
    pub peer_last_measured_y_in_msg: usize,
    /// Row count of the rendered peer block. Same provenance + use
    /// as `peer_last_measured_y_in_msg`. Zero ⇒ no hit-test target.
    pub peer_last_measured_height: usize,
    /// Width the peer block was laid out at. Used to invalidate the
    /// hit-target when the chat area resizes (a stale rect from a
    /// previous width would mis-route clicks).
    pub peer_last_measured_width: u16,
}

impl TextBlock {
    pub fn new(text: String) -> Self {
        Self {
            markdown: IncrementalMarkdown::from_complete(&text),
            text,
            cache: BlockCache::default(),
            trailing_spacing: TextBlockSpacing::None,
            peer_collapsed_override: None,
            peer_last_measured_y_in_msg: 0,
            peer_last_measured_height: 0,
            peer_last_measured_width: 0,
        }
    }

    pub fn from_complete(text: &str) -> Self {
        Self::new(text.to_owned())
    }

    pub fn with_trailing_spacing(mut self, trailing_spacing: TextBlockSpacing) -> Self {
        self.trailing_spacing = trailing_spacing;
        self
    }

    pub fn trailing_blank_lines(&self) -> usize {
        self.trailing_spacing.blank_lines()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateLimitIncidentKey {
    pub rate_limit_type: Option<String>,
    pub resets_at_bucket: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NoticeDedupKey {
    RateLimit(RateLimitIncidentKey),
    ApiRetry,
}

pub struct NoticeBlock {
    pub severity: SystemSeverity,
    pub text: TextBlock,
    pub dedup_key: Option<NoticeDedupKey>,
}

impl NoticeBlock {
    pub fn new(severity: SystemSeverity, text: String) -> Self {
        Self { severity, text: TextBlock::new(text), dedup_key: None }
    }

    pub fn from_complete(severity: SystemSeverity, text: &str) -> Self {
        Self::new(severity, text.to_owned())
    }

    pub fn with_dedup_key(mut self, dedup_key: NoticeDedupKey) -> Self {
        self.dedup_key = Some(dedup_key);
        self
    }

    pub fn replace_text(&mut self, text: &str) {
        self.text = TextBlock::from_complete(text);
    }

    pub fn trailing_blank_lines(&self) -> usize {
        self.text.trailing_blank_lines()
    }
}

/// Ordered content block - text and tool calls interleaved as they arrive.
pub enum MessageBlock {
    Text(TextBlock),
    Notice(NoticeBlock),
    ToolCall(Box<ToolCallInfo>),
    Welcome(WelcomeBlock),
    /// Indicates N images were attached to this user message.
    ImageAttachment(ImageAttachmentBlock),
}

/// Lightweight block for image attachment indicators. Carries a [`BlockCache`]
/// to satisfy the render-budget invariant that every [`MessageBlock`] variant
/// has a cache, even though the cached content is trivially small.
pub struct ImageAttachmentBlock {
    pub count: usize,
    pub cache: BlockCache,
}

impl ImageAttachmentBlock {
    pub fn new(count: usize) -> Self {
        Self { count, cache: BlockCache::default() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MessageRole {
    User,
    Assistant,
    System(Option<SystemSeverity>),
    Welcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SystemSeverity {
    Info,
    Warning,
    Error,
}

pub struct WelcomeBlock {
    pub version: String,
    /// Label rendered before the account/subscription value, e.g.
    /// `"Account"` (when forge-workspace picked an account) or
    /// `"Subscription"` (fallback for direct Agent::spawn callers).
    pub account_label: String,
    pub subscription: String,
    pub cwd: String,
    pub session_id: String,
    pub tip_seed: u64,
    pub cache: BlockCache,
}
