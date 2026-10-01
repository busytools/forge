use super::tool_call_info::ToolCallInfo;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// A text block's identity, monotonic per process and never reused, so a
/// cache keyed on one cannot be handed another block's entry.
///
/// A newtype rather than a `u64` because its two neighbours in the store
/// are also `u64`s - the content fold and the message id - and a caller
/// passing the wrong one is otherwise a silent wrong render rather than a
/// build failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub u64);

/// A message's identity, with [`BlockId`]'s guarantee and for the same
/// reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(pub u64);

static NEXT_TEXT_BLOCK_ID: AtomicU64 = AtomicU64::new(1);

fn next_text_block_id() -> BlockId {
    BlockId(NEXT_TEXT_BLOCK_ID.fetch_add(1, Ordering::Relaxed))
}

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

fn next_message_id() -> MessageId {
    MessageId(NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed))
}

pub struct ChatMessage {
    pub role: MessageRole,
    pub blocks: Vec<MessageBlock>,
    /// Stable for the life of the message, and the key the message's
    /// rendered rows are cached under - see [`TextBlock::id`] for why the
    /// key is an identity rather than the content fold.
    pub id: MessageId,
    /// #143 item 2: cached peer-envelope flag stamped once at push
    /// time (the `PeerEnvelopeAppended` reducer + similar entry
    /// points) so the chat renderer doesn't walk text blocks every
    /// frame to recompute it. The flag is intrinsic to EVERY text block
    /// in the message, not just the first: consecutive envelopes merge
    /// into one message and the merge is gated on the constructor, so a
    /// flagged message holds only envelopes of that one kind. Do NOT
    /// read `blocks.first()` to answer "which envelope is this" - a
    /// merged message holds N and you would silently drop N-1.
    pub is_peer_envelope: bool,
    /// Companion to [`Self::is_peer_envelope`] for the Gotify external-
    /// notification variant (`[Gotify - app '...']`). Stamped at push
    /// time by the `GotifyNotificationAppended` path so the role label
    /// renders a distinct `Gotify` source where peer traffic renders
    /// none, without a per-frame `detect_inbound` walk.
    pub is_gotify_envelope: bool,
    /// Companion flag for a fired-cron turn (`[Cron]`). Stamped at push
    /// time by the `CronPromptAppended` path so the role label renders a
    /// distinct `Cron` source where peer traffic renders none.
    pub is_cron_envelope: bool,
    /// Companion flag for an inbound Slack message. Stamped at push time by
    /// the `SlackMessageAppended` path so the role label renders a distinct
    /// `Slack` source where peer traffic renders none.
    pub is_slack_envelope: bool,
    /// #273: stop_hook_summary chip hit-test - wrapped-row offset
    /// inside this message of the clickable chip line(s). `0` when
    /// no chip is rendered. Stamped by `append_stop_hook_summary`.
    pub stop_hook_summary_y_in_msg: usize,
    /// #273: stop_hook_summary chip hit-test - wrapped-row height of
    /// the clickable chip line(s) (excludes the leading blank and
    /// any expanded hook rows). `0` when no chip is rendered.
    pub stop_hook_summary_height: usize,
    /// Accounting for the turn this message closes, rendered as the
    /// message's trailing turn-info row.
    pub turn_info: TurnInfo,
    /// Turn-info row hit-test, all `0` when no row is rendered: the
    /// wrapped-row offset and height of the clickable row (excluding
    /// its expanded body), and the width they were measured at, since
    /// a click against a stale width must be dropped.
    pub turn_info_y_in_msg: usize,
    pub turn_info_height: usize,
    pub turn_info_width: u16,
}

/// Per-turn accounting behind the trailing turn-info row.
///
/// The counts and clocks are optional: the row renders from turn
/// start and fills in as data arrives, and an absent value is
/// rendered as absent or dropped, never as zero.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TurnInfo {
    /// When the turn began: stamped at prompt dispatch, or on the
    /// first assistant frame for a turn forge did not dispatch.
    ///
    /// **Absent from the wire, and it is the one field this shape drops.** An
    /// `Instant` is a monotonic clock reading, meaningless outside the
    /// process that took it, so no client could use it - and the figure a
    /// view draws from it, `elapsed_secs` beside it, crosses.
    #[serde(skip)]
    pub started_at: Option<Instant>,
    /// Whole seconds since `started_at`, refreshed once per render so
    /// the cache key and the layout it guards agree.
    pub elapsed_secs: u64,
    /// Settled wall clock for the turn, from `Result.duration_ms`.
    pub duration_ms: Option<u64>,
    /// This turn's API time. `Result.duration_api_ms` counts up across
    /// the whole session, so the per-turn figure is its delta against
    /// the previous Result.
    pub api_ms: Option<u64>,
    /// Local wall-clock `HH:MM:SS` at which the Result arrived - the
    /// wire carries none, so it is read from the clock on arrival.
    pub ended_at_local: Option<String>,
    /// The same instant as the CLI stamped it, RFC 3339, for a turn read
    /// from a transcript: the row's own clock is the only one there is, and
    /// a view renders it in the reader's zone rather than the record
    /// carrying a formatted string that outlives the zone.
    pub ended_at_utc: Option<String>,
    pub model: Option<String>,
    /// Estimated reasoning tokens for this turn, summed from the
    /// `ThinkingTokens` deltas because the wire's running counter
    /// restarts at every thinking block. Mirrored from the session
    /// field while the turn runs, since that field is cleared at turn
    /// end and the settled row still shows it.
    pub thinking_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Input tokens read back from the prompt cache. There is no
    /// output cache; both cache counters are input.
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written into the prompt cache at a premium.
    pub cache_written_tokens: Option<u64>,
    /// Cost for the whole session so far, not this turn.
    pub session_cost_usd: Option<f64>,
    /// Click-to-expand flag. Lives on the message rather than in a
    /// position-keyed side map so it survives a re-render.
    pub expanded: bool,
}

/// Accumulator for the turn currently in flight.
///
/// The CLI splits one assistant message across a frame per content
/// block and repeats the same usage on each, so totals are keyed by
/// message id rather than summed.
#[derive(Debug, Clone, Default)]
pub struct LiveTurn {
    pub started_at: Option<Instant>,
    /// The turn's estimate so far, from the thinking frames it carried.
    /// The result carries none of its own, so this is the only place a live
    /// row can read one.
    pub thinking_tokens: Option<u64>,
    by_message: std::collections::HashMap<String, LiveUsage>,
}

/// The input-side counters forge can trust from an assistant frame.
///
/// `output_tokens` is absent by design: on an assistant frame it is
/// the streaming `message_start` placeholder, not a count.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveUsage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_written_tokens: u64,
}

impl LiveTurn {
    /// Begin a new turn, discarding the previous turn's frames.
    pub fn start(&mut self, at: Instant) {
        self.started_at = Some(at);
        self.thinking_tokens = None;
        self.by_message.clear();
    }

    /// Record one assistant frame's usage. Repeat frames for the same
    /// `message_id` overwrite rather than add.
    pub fn record(&mut self, message_id: String, usage: LiveUsage) {
        self.by_message.insert(message_id, usage);
    }

    /// Running totals across the distinct assistant messages seen so
    /// far, or `None` before any frame has carried usage.
    pub fn totals(&self) -> Option<LiveUsage> {
        if self.by_message.is_empty() {
            return None;
        }
        Some(self.by_message.values().fold(LiveUsage::default(), |acc, u| LiveUsage {
            input_tokens: acc.input_tokens.saturating_add(u.input_tokens),
            cache_read_tokens: acc.cache_read_tokens.saturating_add(u.cache_read_tokens),
            cache_written_tokens: acc.cache_written_tokens.saturating_add(u.cache_written_tokens),
        }))
    }
}

impl TurnInfo {
    /// True when neither half of the row is known. Both are checked
    /// because they come from different writers - `started_at` from
    /// the live path, `duration_ms` from the Result - and a row
    /// renders on either alone.
    pub fn is_empty(&self) -> bool {
        self.started_at.is_none() && self.duration_ms.is_none()
    }

    /// True once `Message::Result` has landed and the row has stopped
    /// counting up.
    pub fn is_settled(&self) -> bool {
        self.duration_ms.is_some()
    }

    /// Wall-clock ms to display: the settled duration once it exists,
    /// otherwise the live elapsed.
    pub fn elapsed_ms(&self) -> u64 {
        self.duration_ms.unwrap_or(self.elapsed_secs.saturating_mul(1_000))
    }

    /// Time spent outside the API - tools and hooks. `None` rather
    /// than clamped when the split is unsound: concurrent subagent
    /// calls can sum past wall clock.
    pub fn local_ms(&self) -> Option<u64> {
        self.duration_ms?.checked_sub(self.api_ms?)
    }

    /// Share of this turn's input tokens served from the cache, over
    /// every input-side counter.
    pub fn cache_hit_percent(&self) -> Option<u64> {
        let read = self.cache_read_tokens?;
        let total = read
            .checked_add(self.input_tokens.unwrap_or(0))?
            .checked_add(self.cache_written_tokens.unwrap_or(0))?;
        if total == 0 {
            return None;
        }
        Some(read.saturating_mul(100) / total)
    }
}

/// A turn's clock as the turn row writes it: one decimal below a minute,
/// then minutes and seconds, then hours.
pub fn format_turn_duration(ms: u64) -> String {
    const SEC: u64 = 1_000;
    const MIN: u64 = 60 * SEC;
    const HOUR: u64 = 60 * MIN;
    if ms < MIN {
        // One decimal, e.g. 12_400 ms -> "12.4s".
        let whole = ms / SEC;
        let tenths = (ms % SEC) / 100;
        return format!("{whole}.{tenths}s");
    }
    if ms < HOUR {
        let minutes = ms / MIN;
        let seconds = (ms % MIN) / SEC;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = ms / HOUR;
    let minutes = (ms % HOUR) / MIN;
    let seconds = (ms % MIN) / SEC;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

/// A token count as the collapsed rows write it: thousands and millions to
/// one decimal below ten of each, integers above, truncated rather than
/// rounded.
pub fn format_token_count_short(n: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    if n < K {
        return n.to_string();
    }
    if n < M {
        // < 10k -> one decimal (e.g. 1.2k, 9.9k); >= 10k -> integer
        // (e.g. 15k, 999k). Truncation via integer division keeps
        // the chip readable - 1199 reads as 1.1k not 1.2k.
        if n < 10 * K {
            let whole = n / K;
            let tenths = (n / (K / 10)) % 10;
            return format!("{whole}.{tenths}k");
        }
        return format!("{}k", n / K);
    }
    if n < 10 * M {
        let whole = n / M;
        let tenths = (n / (M / 10)) % 10;
        return format!("{whole}.{tenths}M");
    }
    format!("{}M", n / M)
}

/// A token count as the expanded body writes it, with its thousands
/// separated.
pub fn format_token_count_grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

impl ChatMessage {
    pub fn new(role: MessageRole, blocks: Vec<MessageBlock>) -> Self {
        Self {
            role,
            blocks,
            id: next_message_id(),
            is_peer_envelope: false,
            is_gotify_envelope: false,
            is_cron_envelope: false,
            is_slack_envelope: false,
            stop_hook_summary_y_in_msg: 0,
            stop_hook_summary_height: 0,
            turn_info: TurnInfo::default(),
            turn_info_y_in_msg: 0,
            turn_info_height: 0,
            turn_info_width: 0,
        }
    }

    /// Variant of `new` for envelope messages that pre-stamps the
    /// peer-envelope flag. Used by the `PeerEnvelopeAppended`
    /// reducer + any other site that constructs a known-envelope
    /// `ChatMessage`. Avoids the render-time `detect_inbound` walk
    /// over the text blocks.
    pub fn new_peer_envelope(role: MessageRole, blocks: Vec<MessageBlock>) -> Self {
        Self {
            role,
            blocks,
            id: next_message_id(),
            is_peer_envelope: true,
            is_gotify_envelope: false,
            is_cron_envelope: false,
            is_slack_envelope: false,
            stop_hook_summary_y_in_msg: 0,
            stop_hook_summary_height: 0,
            turn_info: TurnInfo::default(),
            turn_info_y_in_msg: 0,
            turn_info_height: 0,
            turn_info_width: 0,
        }
    }

    /// Variant of `new` for a Gotify external notification, pre-stamping
    /// [`Self::is_gotify_envelope`]. Used by the `GotifyNotificationAppended`
    /// path so the role label renders the distinct `Gotify` source.
    pub fn new_gotify_envelope(role: MessageRole, blocks: Vec<MessageBlock>) -> Self {
        Self {
            role,
            blocks,
            id: next_message_id(),
            is_peer_envelope: false,
            is_gotify_envelope: true,
            is_cron_envelope: false,
            is_slack_envelope: false,
            stop_hook_summary_y_in_msg: 0,
            stop_hook_summary_height: 0,
            turn_info: TurnInfo::default(),
            turn_info_y_in_msg: 0,
            turn_info_height: 0,
            turn_info_width: 0,
        }
    }

    /// Variant of `new` for a fired-cron turn, pre-stamping
    /// [`Self::is_cron_envelope`]. Used by the `CronPromptAppended` path
    /// so the role label renders the distinct `Cron` source.
    pub fn new_cron_envelope(role: MessageRole, blocks: Vec<MessageBlock>) -> Self {
        Self {
            role,
            blocks,
            id: next_message_id(),
            is_peer_envelope: false,
            is_gotify_envelope: false,
            is_cron_envelope: true,
            is_slack_envelope: false,
            stop_hook_summary_y_in_msg: 0,
            stop_hook_summary_height: 0,
            turn_info: TurnInfo::default(),
            turn_info_y_in_msg: 0,
            turn_info_height: 0,
            turn_info_width: 0,
        }
    }

    /// Variant of `new` for an inbound Slack message, pre-stamping
    /// [`Self::is_slack_envelope`]. Used by
    /// `push_peer_envelope_user_turn_if_present` so the role label renders
    /// the distinct `Slack` source.
    pub fn new_slack_envelope(role: MessageRole, blocks: Vec<MessageBlock>) -> Self {
        Self {
            role,
            blocks,
            id: next_message_id(),
            is_peer_envelope: false,
            is_gotify_envelope: false,
            is_cron_envelope: false,
            is_slack_envelope: true,
            stop_hook_summary_y_in_msg: 0,
            stop_hook_summary_height: 0,
            turn_info: TurnInfo::default(),
            turn_info_y_in_msg: 0,
            turn_info_height: 0,
            turn_info_width: 0,
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
            })],
        )
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageRenderSignature(pub u64);

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

/// Discriminant tags folded into a block's content hash. Stable values
/// matter: changing one invalidates every previously-cached render, and
/// the distinct values are what stop a Text of N bytes colliding with a
/// Notice of the same length.
pub mod block_tag {
    pub const TEXT: u8 = 0;
    pub const NOTICE: u8 = 1;
    pub const TOOL_CALL: u8 = 2;
    pub const WELCOME: u8 = 3;
    pub const IMAGE_ATTACHMENT: u8 = 4;
}

/// Fold a turn's figures. Several invalidations mutate `turn_info` and
/// nothing else, so both the render signature and the content key fold it
/// here rather than twice.
///
/// `started_at` is deliberately not folded: it reaches the key as
/// `elapsed_secs`, which the render path refreshes immediately before
/// building the key.
pub fn hash_turn_info(info: &TurnInfo, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    info.elapsed_secs.hash(hasher);
    info.duration_ms.hash(hasher);
    info.api_ms.hash(hasher);
    info.ended_at_local.hash(hasher);
    info.model.hash(hasher);
    info.thinking_tokens.hash(hasher);
    info.input_tokens.hash(hasher);
    info.output_tokens.hash(hasher);
    info.cache_read_tokens.hash(hasher);
    info.cache_written_tokens.hash(hasher);
    info.session_cost_usd.map(f64::to_bits).hash(hasher);
    info.expanded.hash(hasher);
}

/// Fold everything about `block` that the message itself owns, with none
/// of the view's inputs. The render signature folds this and then the
/// view's own frame and mode, so the content fields are listed once.
pub fn hash_message_block_content_into<H: std::hash::Hasher>(hasher: &mut H, block: &MessageBlock) {
    use std::hash::Hash;
    match block {
        MessageBlock::Text(block) => {
            block_tag::TEXT.hash(hasher);
            block.content_signature().hash(hasher);
        }
        MessageBlock::Notice(block) => {
            block_tag::NOTICE.hash(hasher);
            block.content_signature().hash(hasher);
        }
        MessageBlock::ToolCall(tc) => {
            block_tag::TOOL_CALL.hash(hasher);
            tc.render_epoch.hash(hasher);
            tc.layout_epoch.hash(hasher);
            tc.hidden.hash(hasher);
            tc.status.hash(hasher);
            tc.sdk_tool_name.hash(hasher);
            // Per-tool collapse override flips the rendered shape, so it
            // has to be folded in alongside the global `tools_collapsed`
            // bit (which lives on `MessageRenderCacheKey`).
            tc.collapsed_override.hash(hasher);
        }
        MessageBlock::Welcome(block) => {
            block_tag::WELCOME.hash(hasher);
            hash_welcome_block_content(block).hash(hasher);
        }
        MessageBlock::ImageAttachment(block) => {
            block_tag::IMAGE_ATTACHMENT.hash(hasher);
            block.count.hash(hasher);
        }
    }
}

fn random_welcome_tip_seed() -> u64 {
    let mut hasher = DefaultHasher::new();
    SystemTime::now().duration_since(UNIX_EPOCH).ok().hash(&mut hasher);
    hasher.finish()
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
    /// Stable for the life of the block, and the key both of this block's
    /// caches use: a content key changes on every chunk, so a streaming
    /// block would get one entry per append, and two blocks carrying the
    /// same text in different roles would share a single entry.
    pub id: BlockId,
    pub text: String,
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
    /// Row offset within the rendered message of this block's click
    /// target - the peer card, or the bundle summary row when the block
    /// leads an L2 messaging-group segment. Stamped by the user-block
    /// renderer; zero for non-peer text blocks and for blocks an L2
    /// summary hides.
    pub peer_last_measured_y_in_msg: usize,
    /// Row count of that click target. Zero ⇒ no hit-test target.
    pub peer_last_measured_height: usize,
    /// Width the peer block was laid out at. Used to invalidate the
    /// hit-target when the chat area resizes (a stale rect from a
    /// previous width would mis-route clicks).
    pub peer_last_measured_width: u16,
}

impl TextBlock {
    /// Drop the click target of an inbound peer envelope an L2 summary
    /// has collapsed away, so a click on the summary's rows cannot
    /// resolve to it. Twin of `ToolCallInfo::clear_hit_test_rect`.
    pub fn clear_peer_hit_test_rect(&mut self) {
        self.peer_last_measured_width = 0;
        self.peer_last_measured_height = 0;
        self.peer_last_measured_y_in_msg = 0;
    }

    pub fn new(text: String) -> Self {
        Self {
            id: next_text_block_id(),
            text,
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

    /// This block's own content, folded. The stamp carries it so a change to
    /// the text replaces the block's cached rows; the rows themselves are
    /// keyed by [`Self::id`].
    pub fn content_signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        hash_text_block_content(&self.text, self.trailing_spacing).hash(&mut hasher);
        self.trailing_spacing.hash(&mut hasher);
        // Peer-block collapse state (#114). Without this in the signature,
        // flipping `peer_collapsed_override` from a click handler is a
        // no-op visually because the cached layout is reused.
        self.peer_collapsed_override.hash(&mut hasher);
        hasher.finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitIncidentKey {
    pub rate_limit_type: Option<String>,
    pub resets_at_bucket: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// This notice's own content, folded for the cache key. Keyed on the
    /// TEXT it wraps rather than on the notice, because the notice adds only
    /// the severity tint - and the text is what the rows are built from.
    pub fn content_signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.severity.hash(&mut hasher);
        self.text.content_signature().hash(&mut hasher);
        hasher.finish()
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

/// Lightweight block for image attachment indicators: a count and nothing
/// else, because it renders one trivial row and caches no rows of its own.
pub struct ImageAttachmentBlock {
    pub count: usize,
}

impl ImageAttachmentBlock {
    pub fn new(count: usize) -> Self {
        Self { count }
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
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use forge_primitives::{Message, Usage};

    use super::{LiveTurn, LiveUsage};

    /// The counters a frame carries, in the three the live turn keeps.
    fn usage(input: u64, read: u64, written: u64) -> LiveUsage {
        LiveUsage { input_tokens: input, cache_read_tokens: read, cache_written_tokens: written }
    }

    /// A frame counted once per message, not once per frame.
    ///
    /// The CLI splits one API call across a frame per content block and
    /// repeats the whole call's usage on every one, so a rule that sums
    /// frames multiplies a call by the blocks it drew as. Measured over the
    /// shipped captures: 294 assistant frames carrying usage over 201
    /// message ids.
    #[test]
    fn a_message_counts_once_however_many_frames_carry_its_usage() {
        let mut turn = LiveTurn::default();
        turn.start(std::time::Instant::now());
        for _ in 0..3 {
            turn.record("msg-1".to_owned(), usage(100, 1_000, 10));
        }
        turn.record("msg-2".to_owned(), usage(200, 2_000, 20));

        let totals = turn.totals().expect("both messages reported usage");
        assert_eq!(
            totals.input_tokens, 300,
            "the input side counts each message once and sums the messages, not the frames"
        );
        assert_eq!(
            totals.cache_read_tokens, 3_000,
            "the cache-read counter is keyed by message id the same way"
        );
        assert_eq!(totals.cache_written_tokens, 30, "and so is the cache-write counter");
    }

    /// Last-wins on a repeat, which is the half the wire does not decide.
    ///
    /// No shipped capture repeats a message id with differing usage (0 of the
    /// 93 repeat frames), so nothing in the data says which frame should win
    /// when one does. Pinned because the client's fold copies this rule: a
    /// tiebreak only one side changes is a divergence no capture would show.
    #[test]
    fn a_repeated_id_overwrites_rather_than_adding() {
        let mut turn = LiveTurn::default();
        turn.start(std::time::Instant::now());
        turn.record("msg-1".to_owned(), usage(100, 1_000, 10));
        turn.record("msg-1".to_owned(), usage(250, 2_500, 25));

        let totals = turn.totals().expect("the message reported usage");
        assert_eq!(
            totals.input_tokens, 250,
            "a repeat overwrites: the last frame's input count is the one kept, not the sum"
        );
        assert_eq!(
            totals.cache_read_tokens, 2_500,
            "the cache-read counter keeps the last frame's figure too"
        );
        assert_eq!(totals.cache_written_tokens, 25, "and so does the cache-write counter");
    }

    /// Every capture file under `dir`, at any depth: the version directory is
    /// named for the CLI, and a capture set can sit a level below it.
    fn collect_captures(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("the baseline directory") {
            let path = entry.expect("a baseline entry").path();
            if path.is_dir() {
                collect_captures(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }

    /// One line of a capture file: which way it went and the wire line.
    #[derive(serde::Deserialize)]
    struct CaptureEnvelope {
        dir: String,
        line: String,
    }

    /// The assumption last-wins rests on, checked against the shipped wire.
    ///
    /// `record` overwriting is only free while a repeat carries the same
    /// usage as the frame before it. This is the tripwire on that: the day a
    /// capture shows a differing repeat, the tiebreak starts deciding a real
    /// case and has to be chosen deliberately on both sides of the rule.
    #[test]
    fn no_shipped_capture_repeats_a_message_id_with_different_usage() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../forge-test-harness/baselines/sdk");
        let mut captures = Vec::new();
        collect_captures(&dir, &mut captures);
        assert!(
            captures.len() > 40,
            "the sweep has the captures to read: found {} under {}",
            captures.len(),
            dir.display()
        );

        let mut frames = 0usize;
        let mut repeats = 0usize;
        let mut differing = Vec::new();
        for path in &captures {
            let raw = std::fs::read_to_string(path).expect("the capture");
            let mut seen: HashMap<String, Usage> = HashMap::new();
            // Counted per file so one capture going unreadable cannot hide
            // behind the others' frames.
            let mut carried = 0usize;
            let mut decoded = 0usize;
            for envelope in
                raw.lines().filter_map(|line| serde_json::from_str::<CaptureEnvelope>(line).ok())
            {
                if envelope.dir != "in" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&envelope.line) else {
                    continue;
                };
                if value["type"] == "assistant" {
                    carried += 1;
                }
                let Ok(Message::Assistant { message, .. }) =
                    serde_json::from_value::<Message>(value)
                else {
                    continue;
                };
                decoded += 1;
                let Some(usage) = message.usage else {
                    continue;
                };
                frames += 1;
                match seen.get(&message.id) {
                    Some(before) if *before != usage => {
                        differing.push(format!("{}: {}", path.display(), message.id));
                    }
                    Some(_) => repeats += 1,
                    None => {
                        seen.insert(message.id.clone(), usage);
                    }
                }
            }
            assert_eq!(decoded, carried, "every assistant frame in {} decoded", path.display());
        }

        assert!(
            frames > 100,
            "the sweep read the captures: {frames} assistant frames carried usage"
        );
        assert!(repeats > 10, "and met repeats: {repeats}");
        assert!(
            differing.is_empty(),
            "no shipped capture repeats a message id with different usage: {differing:?}"
        );
    }
}
