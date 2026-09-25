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
#[derive(Debug, Clone, Default)]
pub struct TurnInfo {
    /// When the turn began: stamped at prompt dispatch, or on the
    /// first assistant frame for a turn forge did not dispatch.
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
