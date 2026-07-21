//! Peer-coordination chat blocks (#114 v1).
//!
//! Two responsibilities:
//!
//! 1. **Inbound detection + rendering**. Pattern-match the bracket-
//!    wrapped prose `forge_workspace::deliver_peer_prompt` injects
//!    into user-turn text (e.g. `[Question id=q-... from
//!    agent 'forge' (org 'Personal') - reply with peers__tell_agent
//!    in_reply_to=q-...]\n\n<body>`) and render a styled block in
//!    place of the default user-message bubble. Catches the five
//!    peer/worker kinds the workspace produces (`Question`, `Message`,
//!    `Reply`, `DeliveryFailure`, `WorkerSpawnFailed`) plus
//!    the inbound `Gotify` external-notification block, which renders
//!    with distinct chrome (the ◈ gotify glyph + a `Gotify` source label).
//!
//! 2. **Outbound rendering**. Replace the default tool_use card for
//!    `mcp__forge__peers__ask_agent` / `peers__tell_agent` /
//!    `workers__ask` / `workers__tell` with a one-line
//!    `▶ Verb name` row + a body preview pulled from the tool
//!    arguments. `workers__spawn` / `workers__list` are NOT handled
//!    here - they render as standard tool cards because they're
//!    worker-lifecycle tool calls, not peer comms.
//!
//! Pure rendering - no I/O, no state. Each call parses the text
//! fresh; results aren't cached (text is small, render frames don't
//! call this hot enough to need a cache).
//!
//! Visual reference: `docs/forge-map.html#peer-block`.

use crate::app::ToolCallInfo;
use crate::ui::chat_tree;
use crate::ui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// One inbound peer block parsed from the user-turn text.
///
/// Wire envelopes carry several fields (correlation id, originating
/// org) that the previous chrome surfaced as DIM meta chunks. The
/// redesigned chat block hides those by default - the parser still
/// skips past them in the prefix, but the type only retains what the
/// renderer or chat-streak grouping reads.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PeerInboundKind {
    Question {
        from: String,
        org: String,
        body: String,
    },
    Message {
        from: String,
        org: String,
        body: String,
    },
    Reply {
        from: String,
        org: String,
        body: String,
    },
    /// `[Ask id=... to agent 'X' (org 'Y') failed to deliver: <reason>]`
    /// Caller-side delivery failure (spawn / connection / channel).
    DeliveryFailure {
        target: String,
        org: String,
        reason: String,
    },
    /// `[Worker '<label>' spawn failed id=<id>: <reason>]`
    /// Lead-side notice that a team worker's async spawn failed
    /// (subprocess crashed inside the `--worktree` machinery before
    /// reaching `Connected`). Reason text is verbatim from claude's
    /// stderr. Kept as a one-line system notice rather than a peer
    /// row because it's a workspace-generated lifecycle event, not a
    /// peer comm - touching its render shape is out of scope for
    /// #189.
    WorkerSpawnFailed {
        label: String,
        reason: String,
    },
    /// `[Gotify - app 'X', priority N]\n<title>\n<message>` - an inbound
    /// external Gotify notification delivered as a user turn. Rendered
    /// with distinct chrome (the ◈ gotify glyph, `Gotify` source label) so it
    /// reads as an external event, not agent traffic. Never groups with
    /// peer envelopes (see [`PeerInboundKind::peer_sender_identity`]).
    Gotify {
        app: String,
        title: String,
        message: String,
        priority: u8,
    },
    /// `[Cron]\n\n<prompt>` - a durable cron that fired into this session,
    /// delivered as a user turn. Rendered with the ◴ cron glyph + a `Cron`
    /// source label so it reads as a scheduled internal event, not agent
    /// traffic. Never groups with peer envelopes (see
    /// [`PeerInboundKind::peer_sender_identity`]).
    Cron {
        prompt: String,
    },
}

impl PeerInboundKind {
    /// The `sender_org` field threaded through every variant - drives
    /// same-project envelope grouping at the chat-iteration level
    /// (see `crate::ui::chat`). Variants that carry an explicit org
    /// return it; the worker-spawn-failure notice has no org of its
    /// own (it's lead-local, with no sending project) so it returns
    /// an empty string and naturally groups with adjacent lead-local
    /// envelopes.
    pub(crate) fn org(&self) -> &str {
        match self {
            Self::Question { org, .. }
            | Self::Message { org, .. }
            | Self::Reply { org, .. }
            | Self::DeliveryFailure { org, .. } => org,
            Self::WorkerSpawnFailed { .. } | Self::Gotify { .. } | Self::Cron { .. } => "",
        }
    }

    /// The peer/worker sender identity used for envelope grouping (the
    /// `from` / `target` / `label` name). `None` for the `Gotify`
    /// external-notification variant, which is not agent traffic and must
    /// never merge into a peer messaging group - the grouping predicates
    /// key off `Some(..)` here.
    pub(crate) fn peer_sender_identity(&self) -> Option<&str> {
        match self {
            Self::Question { from, .. } | Self::Message { from, .. } | Self::Reply { from, .. } => {
                Some(from)
            }
            Self::DeliveryFailure { target, .. } => Some(target),
            Self::WorkerSpawnFailed { label, .. } => Some(label),
            Self::Gotify { .. } | Self::Cron { .. } => None,
        }
    }
}

/// One outbound peer or worker block parsed from a `mcp__forge__peers__*`
/// or `mcp__forge__workers__ask|tell` tool_use card. The redesigned
/// chrome drops the family / correlation_id chrome - both peer and
/// worker calls render with the same `▶ Verb name` shape.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PeerOutboundKind {
    Ask { target: String, body: String },
    Tell { target: String, body: String },
}

/// Modifier suffix surfaced inline after the `Verb name` header when
/// the envelope is a notice variant. Renders as ` - ⚠ <label>` in
/// `STATUS_WARNING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeModifier {
    Undeliverable,
}

impl NoticeModifier {
    const fn label(self) -> &'static str {
        match self {
            Self::Undeliverable => "undeliverable",
        }
    }
}

/// Detect a peer wrapper at the start of a user-message text. Returns
/// `None` for any text that isn't a bracket-prefixed peer wrapper
/// (the chat renderer falls through to the default text rendering
/// in that case).
pub(crate) fn detect_inbound(text: &str) -> Option<PeerInboundKind> {
    let bracketed = text.strip_prefix('[')?;
    let close_idx = bracketed.find(']')?;
    let header = &bracketed[..close_idx];
    let after_bracket = &bracketed[close_idx + 1..];
    // The wrapper formats land the body after `]\n\n`. Some notice
    // variants have an empty body - the bracket itself is the whole
    // message, possibly followed by `\n\n` + extra context. Both
    // are valid.
    let body = after_bracket.strip_prefix("\n\n").unwrap_or("").to_owned();

    if let Some(rest) = header.strip_prefix("Question id=") {
        let (_id, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        return Some(PeerInboundKind::Question { from, org, body });
    }

    if let Some(rest) = header.strip_prefix("Message id=") {
        let (_id, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        return Some(PeerInboundKind::Message { from, org, body });
    }

    if let Some(rest) = header.strip_prefix("Reply id=") {
        let (_id, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        return Some(PeerInboundKind::Reply { from, org, body });
    }

    if let Some(rest) = header.strip_prefix("Ask id=") {
        // Caller-side delivery failure - `to agent 'X' (org 'Y') failed to deliver: <reason>`
        if let Some(rest_to) = rest_after_id(rest, " to agent ")
            && header.contains("failed to deliver:")
        {
            let (target, org, trailing) = extract_from_agent_after_with_trailer(rest_to)?;
            let reason = trailing
                .split_once("failed to deliver:")
                .map(|(_, after)| after.trim().to_owned())
                .unwrap_or_default();
            return Some(PeerInboundKind::DeliveryFailure { target, org, reason });
        }
    }

    if let Some(rest) = header.strip_prefix("Worker '") {
        let (label, rest) = take_until(rest, "' spawn failed id=")?;
        let (_id, reason) = take_until(rest, ": ")?;
        return Some(PeerInboundKind::WorkerSpawnFailed {
            label: label.to_owned(),
            reason: reason.to_owned(),
        });
    }

    if let Some(rest) = header.strip_prefix("Gotify - app '") {
        let (app, priority_str) = take_until(rest, "', priority ")?;
        let priority: u8 = priority_str.parse().ok()?;
        // The Gotify prose places the body one newline after `]` (title on
        // its own line, then the message) - not the peer `]\n\n` shape.
        let raw_body = after_bracket.strip_prefix('\n').unwrap_or(after_bracket);
        let (title, message) = match raw_body.split_once('\n') {
            Some((t, m)) => (t.to_owned(), m.to_owned()),
            None => (raw_body.to_owned(), String::new()),
        };
        return Some(PeerInboundKind::Gotify { app: app.to_owned(), title, message, priority });
    }

    if header == "Cron" {
        // Display-only wrapper the CronPromptAppended reducer forges around
        // the fired prompt; `body` is the raw prompt after `]\n\n`.
        return Some(PeerInboundKind::Cron { prompt: body });
    }

    None
}

/// Detect a peer / worker outbound tool_use card. Returns `None` for
/// every other tool (the chat renderer falls through to the default
/// tool-card rendering) and explicitly for `workers__spawn` /
/// `workers__list` - those are worker-lifecycle tool calls that render
/// as standard tool cards rather than peer comms.
pub(crate) fn detect_outbound(tc: &ToolCallInfo) -> Option<PeerOutboundKind> {
    let raw = tc.raw_input.as_ref()?;
    match tc.sdk_tool_name.as_str() {
        "mcp__forge__peers__ask_agent" => {
            let target = raw.get("target")?.as_str()?.to_owned();
            let body = raw.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Ask { target, body })
        }
        "mcp__forge__peers__tell_agent" => {
            let target = raw.get("target")?.as_str()?.to_owned();
            let body = raw.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Tell { target, body })
        }
        "mcp__forge__workers__ask" => {
            let target = raw.get("label")?.as_str()?.to_owned();
            let body = raw.get("question").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Ask { target, body })
        }
        "mcp__forge__workers__tell" => {
            let target = raw.get("label")?.as_str()?.to_owned();
            let body = raw.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Tell { target, body })
        }
        _ => None,
    }
}

/// Build the styled lines for an inbound peer block.
///
/// `suppress_header = true` is the same-worker streak-follower case:
/// the `▶ Verb name` line is dropped and only the body lines render,
/// so consecutive messages from the same worker stack as one
/// paragraph.
pub(crate) fn render_inbound(
    kind: &PeerInboundKind,
    suppress_header: bool,
    collapsed: bool,
) -> Vec<Line<'static>> {
    match kind {
        PeerInboundKind::Question { from, body, .. } => {
            render_block("Question", from, None, body, INBOUND_GLYPH, suppress_header, collapsed)
        }
        PeerInboundKind::Message { from, body, .. } => {
            render_block("Message", from, None, body, INBOUND_GLYPH, suppress_header, collapsed)
        }
        PeerInboundKind::Reply { from, body, .. } => {
            render_block("Reply", from, None, body, INBOUND_GLYPH, suppress_header, collapsed)
        }
        PeerInboundKind::DeliveryFailure { target, reason, .. } => render_block(
            "Ask",
            target,
            Some(NoticeModifier::Undeliverable),
            reason,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
        ),
        PeerInboundKind::WorkerSpawnFailed { label, reason } => {
            render_worker_spawn_failed(label, reason)
        }
        PeerInboundKind::Gotify { app, title, message, priority } => {
            render_gotify_notification(app, *priority, title, message, suppress_header, collapsed)
        }
        PeerInboundKind::Cron { prompt } => render_cron_prompt(prompt, suppress_header, collapsed),
    }
}

/// Build the styled lines for an outbound peer / worker block.
pub(crate) fn render_outbound(kind: &PeerOutboundKind, collapsed: bool) -> Vec<Line<'static>> {
    match kind {
        PeerOutboundKind::Ask { target, body } => {
            render_block("Ask", target, None, body, OUTBOUND_GLYPH, false, collapsed)
        }
        PeerOutboundKind::Tell { target, body } => {
            render_block("Tell", target, None, body, OUTBOUND_GLYPH, false, collapsed)
        }
    }
}

/// Header glyph for every chat block. Distinct enough from the
/// standard tool-card glyphs (`✓` / `⚠` / `✗`) to read as "this is a
/// peer / worker row, not a tool call".
const ROW_GLYPH: &str = "\u{25B6}"; // ▶

/// Directional kind-icon for outbound rows (Ask / Tell). U+2934
/// CURVED ARROW POINTING RIGHTWARDS AND CURVING UPWARDS.
const OUTBOUND_GLYPH: &str = "\u{2934}";

/// Directional kind-icon for inbound rows (Question / Message /
/// Reply / DeliveryFailure). U+2935 CURVED ARROW POINTING RIGHTWARDS
/// AND CURVING DOWNWARDS.
const INBOUND_GLYPH: &str = "\u{2935}";

/// Kind-icon for an inbound Gotify notification. U+25C8 - the shared
/// gotify glyph (also the Inspector GOTIFY status line), so gotify reads
/// with one icon everywhere; distinct from the `ROW_GLYPH` peer / worker
/// rows use. Monochrome + width-1, matching the codebase glyph convention.
const GOTIFY_GLYPH: &str = "\u{25C8}";

/// Kind-icon for a fired-cron block. U+25F4 CIRCLE WITH UPPER LEFT
/// QUADRANT - the same glyph the Inspector SCHEDULES section uses for a
/// cron, so cron reads with one icon everywhere.
const CRON_GLYPH: &str = "\u{25f4}";

/// A Gotify priority at or above this renders in `STATUS_WARNING` (a
/// severity cue); below it stays `DIM`. Gotify priorities run 0-10.
const GOTIFY_ELEVATED_PRIORITY: u8 = 5;

/// Unified renderer for the new chat-block shape:
///
/// ```text
///   ▶ ⤴ Verb name[ - ⚠ modifier]   (outbound)
///   ▶ ⤵ Verb name[ - ⚠ modifier]   (inbound)
///   │  body line 1
///   └─ body line 2
/// ```
///
/// `direction_glyph` is the leading kind-icon - `OUTBOUND_GLYPH` for
/// `render_outbound` callers, `INBOUND_GLYPH` for every `render_inbound`
/// arm. `suppress_header = true` drops the header row entirely (same-
/// worker streak follower) and the glyph goes with it.
fn render_block(
    verb: &str,
    name: &str,
    modifier: Option<NoticeModifier>,
    body: &str,
    direction_glyph: &str,
    suppress_header: bool,
    collapsed: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !suppress_header {
        let mut header = Line::default();
        header.spans.push(Span::raw("  "));
        header.spans.push(Span::styled(
            ROW_GLYPH.to_owned(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ));
        header.spans.push(Span::raw(" "));
        header.spans.push(Span::styled(
            direction_glyph.to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        ));
        header.spans.push(Span::raw(" "));
        header.spans.push(Span::styled(
            format!("{verb} {name}"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        if let Some(m) = modifier {
            header.spans.push(Span::styled(" - ".to_owned(), Style::default().fg(theme::DIM)));
            header.spans.push(Span::styled(
                format!("\u{26a0} {}", m.label()),
                Style::default().fg(theme::STATUS_WARNING),
            ));
        }
        lines.push(header);
    }
    if collapsed {
        push_collapsed_summary(&mut lines, body);
    } else {
        push_tree_body_lines(&mut lines, body);
    }
    lines
}

/// One-off renderer for the `WorkerSpawnFailed` lifecycle notice.
/// Kept distinct from `render_block` because the spawn failure is a
/// workspace-generated system event, not a peer comm; carrying it
/// through the verb-row shape would force a non-fitting verb. The
/// ✗-glyph + plain prose treatment signals "system notice, not a
/// peer row" at a glance.
fn render_worker_spawn_failed(label: &str, reason: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut header = Line::default();
    header.spans.push(Span::raw("  "));
    header.spans.push(Span::styled(
        "\u{2717}".to_owned(),
        Style::default().fg(theme::STATUS_ERROR).add_modifier(Modifier::BOLD),
    ));
    header.spans.push(Span::raw(" "));
    header.spans.push(Span::styled(
        format!("Worker '{label}' spawn failed"),
        Style::default().fg(theme::STATUS_ERROR).add_modifier(Modifier::BOLD),
    ));
    lines.push(header);
    push_tree_body_lines(&mut lines, reason);
    lines
}

/// Render an inbound Gotify notification block. Distinct chrome from the
/// peer rows: the ◈ gotify glyph + `app 'X' - priority N` header (priority
/// in `STATUS_WARNING` at or above [`GOTIFY_ELEVATED_PRIORITY`], else
/// `DIM`), body = title then message under the standard tree connectors.
/// `suppress_header` drops the header line (streak follower); Gotify
/// turns stand alone in practice, so it's normally rendered in full.
fn render_gotify_notification(
    app: &str,
    priority: u8,
    title: &str,
    message: &str,
    suppress_header: bool,
    collapsed: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !suppress_header {
        let priority_style = if priority >= GOTIFY_ELEVATED_PRIORITY {
            Style::default().fg(theme::STATUS_WARNING)
        } else {
            Style::default().fg(theme::DIM)
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                GOTIFY_GLYPH.to_owned(),
                Style::default().fg(theme::GOTIFY).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(format!("app '{app}'"), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(" - priority ".to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(format!("{priority}"), priority_style),
        ]));
    }
    let body = if message.is_empty() { title.to_owned() } else { format!("{title}\n{message}") };
    if collapsed {
        push_collapsed_summary(&mut lines, &body);
    } else {
        push_tree_body_lines(&mut lines, &body);
    }
    lines
}

/// Render a fired-cron block. Distinct chrome from peer rows: the ◴ cron
/// glyph + a `Cron` source label (the SCHEDULES-section cron glyph), body =
/// the fired prompt under the standard tree connectors. `suppress_header`
/// drops the header line (streak follower); cron turns stand alone in
/// practice, so it's normally rendered in full.
fn render_cron_prompt(prompt: &str, suppress_header: bool, collapsed: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !suppress_header {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                CRON_GLYPH.to_owned(),
                Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled("Cron", Style::default().add_modifier(Modifier::BOLD)),
        ]));
    }
    if collapsed {
        push_collapsed_summary(&mut lines, prompt);
    } else {
        push_tree_body_lines(&mut lines, prompt);
    }
    lines
}

/// One-line collapsed summary shape: `  └─ <first line of body, truncated>  click or ctrl+x to expand`.
/// Skips entirely when the body is empty so notice variants (which
/// have no prose body) don't render an orphan `└─ click to expand`
/// row pointing at nothing.
fn push_collapsed_summary(lines: &mut Vec<Line<'static>>, body: &str) {
    // First non-blank line, truncated to a short width so the summary
    // fits on one terminal row. Matches the standard tool card's
    // collapsed summary length (`DEFAULT_COLLAPSED_TEXT_SUMMARY_LIMIT`
    // = 60 chars).
    const SUMMARY_LIMIT: usize = 60;
    let body = body.trim();
    if body.is_empty() {
        return;
    }
    let head = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let summary: String = if head.chars().count() > SUMMARY_LIMIT {
        let mut s: String = head.chars().take(SUMMARY_LIMIT).collect();
        s.push('\u{2026}');
        s
    } else {
        head.to_owned()
    };

    let dim = Style::default().fg(theme::DIM);
    let mut line = Line::default();
    line.spans.push(Span::styled(format!("  {}", chat_tree::LAST), dim));
    line.spans.push(Span::styled(summary, dim));
    line.spans.push(Span::styled("  click or ctrl+x to expand".to_owned(), dim));
    lines.push(line);
}

/// Push the body lines under `│  ` / `└─ ` tree connectors - matches
/// `tool_call::standard::render_tool_content`'s pipe / corner glyph
/// pair. Renders the FULL body when expanded - no truncation; the
/// collapsed summary (see [`push_collapsed_summary`]) is the only
/// place we truncate, and only to fit a single summary row. When the
/// body is empty, pushes nothing so the header stands alone.
fn push_tree_body_lines(lines: &mut Vec<Line<'static>>, body: &str) {
    let body = body.trim();
    if body.is_empty() {
        return;
    }

    let pipe_style = Style::default().fg(theme::DIM);
    let body_text_style = Style::default().fg(Color::Gray);

    let body_lines: Vec<&str> = body.lines().collect();
    let last_idx = body_lines.len().saturating_sub(1);
    for (idx, raw_line) in body_lines.iter().enumerate() {
        let prefix = if idx == last_idx {
            format!("  {}", chat_tree::LAST)
        } else {
            format!("  {}  ", chat_tree::SPINE)
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, pipe_style),
            Span::styled((*raw_line).to_owned(), body_text_style),
        ]));
    }
}

// ---------- parsing helpers ----------

/// Split `s` at the first occurrence of `marker`. Returns
/// `(before, after_marker)` - the marker itself is consumed.
/// `None` when the marker isn't found.
fn take_until<'a>(s: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
    let idx = s.find(marker)?;
    Some((&s[..idx], &s[idx + marker.len()..]))
}

/// After the `id=` slice has been peeled off, look for the marker
/// that comes after the id (e.g. ` to agent ` or ` from agent `).
/// Returns the slice after the marker.
fn rest_after_id<'a>(after_id: &'a str, marker: &str) -> Option<&'a str> {
    let idx = after_id.find(marker)?;
    Some(&after_id[idx + marker.len()..])
}

/// Given a slice starting at `'X' (org 'Y') ...`, extract `X` and
/// `Y`. Returns `None` when the format doesn't match.
fn extract_from_agent_after(rest: &str) -> Option<(String, String)> {
    let after_open = rest.strip_prefix('\'')?;
    let (name, after_name) = take_until(after_open, "' (org '")?;
    let (org, _trailing) = take_until(after_name, "')")?;
    Some((name.to_owned(), org.to_owned()))
}

/// Same as `extract_from_agent_after` but also returns the trailer -
/// the substring after `')`. Used by the delivery-failure parser
/// which needs name, org, AND the trailing reason text.
fn extract_from_agent_after_with_trailer(rest: &str) -> Option<(String, String, String)> {
    let after_open = rest.strip_prefix('\'')?;
    let (name, after_name) = take_until(after_open, "' (org '")?;
    let (org, trailing) = take_until(after_name, "')")?;
    Some((name.to_owned(), org.to_owned(), trailing.to_owned()))
}

/// Render the L2 summary line for a messaging group: 2-space indent
/// + status_icon + `@ ` (BOLD DIM) + BOLD heading + DIM ctrl+x hint.
///
/// Heading shape: `<n> message(s)` followed by direction-qualified
/// target clauses (`· outbound to <targets>` and/or
/// `· inbound from <targets>`). Targets render in order of first
/// appearance; `+N` overflow appends after the named list. Direction
/// clauses with no targets are omitted entirely (no "0 inbound"
/// filler).
///
/// The aggregate status drives the leading icon via the same
/// `tool_call::status_icon` helper the per-tool render uses; the
/// active spinner glyph animates on `InProgress`.
pub(crate) fn render_messaging_group_summary_line(
    segment: &crate::ui::message::grouping::MessagingGroupSegment,
    spinner_glyph: char,
) -> Vec<Line<'static>> {
    let (icon_glyph, icon_color) =
        crate::ui::tool_call::status_icon(segment.aggregate_status, spinner_glyph);
    let dim = Style::default().fg(theme::DIM);

    let count = segment.group_total_count.max(segment.segment_count);
    let count_word = if count == 1 { "message" } else { "messages" };
    let mut heading = format!("{count} {count_word}");
    if !segment.segment_outbound_targets.is_empty() {
        heading.push_str(" \u{b7} outbound to ");
        heading.push_str(&format_direction_targets(&segment.segment_outbound_targets));
    }
    if !segment.segment_inbound_targets.is_empty() {
        heading.push_str(" \u{b7} inbound from ");
        heading.push_str(&format_direction_targets(&segment.segment_inbound_targets));
    }

    vec![Line::from(vec![
        Span::raw("  ".to_owned()),
        Span::styled(
            format!("{icon_glyph} "),
            Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("@ ".to_owned(), Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD)),
        Span::styled(heading, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("   ctrl+x to expand".to_owned(), dim),
    ])]
}

fn format_direction_targets(
    targets: &crate::ui::message::grouping::MessagingDirectionTargets,
) -> String {
    let mut out = targets.targets.join(", ");
    if targets.overflow_n > 0 {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push('+');
        out.push_str(&targets.overflow_n.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_question_inbound() {
        let text = "[Question id=q-7f3a92e0 from agent 'forge' (org 'Personal') - reply with peers__tell_agent in_reply_to=q-7f3a92e0]\n\nWhat's the test setup?";
        let kind = detect_inbound(text).expect("question");
        match kind {
            PeerInboundKind::Question { from, org, body } => {
                assert_eq!(from, "forge");
                assert_eq!(org, "Personal");
                assert_eq!(body, "What's the test setup?");
            }
            other => panic!("expected Question, got {other:?}"),
        }
    }

    #[test]
    fn detect_message_inbound() {
        let text = "[Message id=t-c45a8f12 from agent 'gateway-backend' (org 'Gateway')]\n\nFYI rewriter cleanup just landed.";
        let kind = detect_inbound(text).expect("message");
        match kind {
            PeerInboundKind::Message { from, org, body } => {
                assert_eq!(from, "gateway-backend");
                assert_eq!(org, "Gateway");
                assert_eq!(body, "FYI rewriter cleanup just landed.");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn detect_reply_inbound() {
        let text = "[Reply id=q-7f3a92e0 from agent 'gateway-backend' (org 'Gateway') to your earlier ask]\n\nWe use pgtemp for ephemeral postgres in CI.";
        let kind = detect_inbound(text).expect("reply");
        match kind {
            PeerInboundKind::Reply { from, org, body } => {
                assert_eq!(from, "gateway-backend");
                assert_eq!(org, "Gateway");
                assert_eq!(body, "We use pgtemp for ephemeral postgres in CI.");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn detect_delivery_failure_inbound() {
        let text = "[Ask id=q-d31fa8a3 to agent 'gateway-liq-bot' (org 'Gateway') failed to deliver: target spawn failed: all pinned accounts are rate-limited]\n\n";
        let kind = detect_inbound(text).expect("delivery failure");
        match kind {
            PeerInboundKind::DeliveryFailure { target, org, reason } => {
                assert_eq!(target, "gateway-liq-bot");
                assert_eq!(org, "Gateway");
                assert!(reason.contains("rate-limited"), "reason carries failure detail: {reason}");
            }
            other => panic!("expected DeliveryFailure, got {other:?}"),
        }
    }

    #[test]
    fn detect_worker_spawn_failed_inbound() {
        let text = "[Worker 'planner' spawn failed id=plan-7f3a: spawned with --worktree but git CLI not found]\n\n";
        let kind = detect_inbound(text).expect("worker spawn failed");
        match kind {
            PeerInboundKind::WorkerSpawnFailed { label, reason } => {
                assert_eq!(label, "planner");
                assert!(reason.contains("git CLI not found"), "reason text: {reason}");
            }
            other => panic!("expected WorkerSpawnFailed, got {other:?}"),
        }
    }

    #[test]
    fn detect_inbound_rejects_non_peer_text() {
        assert!(detect_inbound("plain user message").is_none());
        assert!(detect_inbound("[not-a-peer-prefix]").is_none());
        assert!(detect_inbound("[Question id=q-bad").is_none());
    }

    fn render_lines_to_strings(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone().into_owned()).collect::<String>())
            .collect()
    }

    #[test]
    fn render_inbound_question_full_shape() {
        let kind = PeerInboundKind::Question {
            from: "planner".into(),
            org: "Personal".into(),
            body: "Is the seam plan ready?".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains("\u{25B6}"), "header has ▶ glyph: {:?}", s[0]);
        assert!(s[0].contains("Question planner"), "verb + name: {:?}", s[0]);
        assert!(!s[0].contains("Personal"), "org suppressed: {:?}", s[0]);
        assert!(s.last().unwrap().contains("Is the seam plan ready?"));
    }

    #[test]
    fn render_inbound_suppress_header_drops_verb_line() {
        let kind = PeerInboundKind::Message {
            from: "implementer".into(),
            org: "Personal".into(),
            body: "PR #187 open.".into(),
        };
        let lines = render_inbound(&kind, true, false);
        let s = render_lines_to_strings(&lines);
        // No verb header, only body lines under tree connectors.
        assert!(!s.iter().any(|line| line.contains("\u{25B6}")), "no ▶ header: {s:?}");
        assert!(s.iter().any(|line| line.contains("PR #187 open.")), "body present: {s:?}");
    }

    #[test]
    fn render_inbound_collapsed_shows_summary_with_hint() {
        let kind = PeerInboundKind::Message {
            from: "planner".into(),
            org: "Personal".into(),
            body: "first line\nsecond line".into(),
        };
        let lines = render_inbound(&kind, false, true);
        let s = render_lines_to_strings(&lines);
        assert!(s.iter().any(|line| line.contains("click or ctrl+x to expand")));
        // Only the first line is surfaced in the summary, not the second.
        assert!(s.iter().any(|line| line.contains("first line")));
        assert!(!s.iter().any(|line| line.contains("second line")));
    }

    #[test]
    fn render_inbound_expanded_keeps_full_body() {
        let kind = PeerInboundKind::Message {
            from: "planner".into(),
            org: "Personal".into(),
            body: "one\ntwo\nthree".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(s.iter().any(|line| line.contains("one")));
        assert!(s.iter().any(|line| line.contains("two")));
        assert!(s.iter().any(|line| line.contains("three")));
    }

    #[test]
    fn render_outbound_ask_shape() {
        let kind = PeerOutboundKind::Ask {
            target: "planner".into(),
            body: "Is the seam plan ready?".into(),
        };
        let lines = render_outbound(&kind, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains("Ask planner"), "verb + target: {:?}", s[0]);
        assert!(s[0].contains("\u{25B6}"), "▶ glyph: {:?}", s[0]);
    }

    #[test]
    fn render_outbound_tell_shape() {
        let kind = PeerOutboundKind::Tell {
            target: "planner".into(),
            body: "FYI: PR #187 is open.".into(),
        };
        let lines = render_outbound(&kind, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains("Tell planner"));
    }

    #[test]
    fn render_outbound_ask_includes_outbound_directional_glyph() {
        let kind = PeerOutboundKind::Ask {
            target: "planner".into(),
            body: "Is the seam plan ready?".into(),
        };
        let lines = render_outbound(&kind, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2934}'),
            "outbound glyph ⤴ U+2934 must appear in header: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_outbound_tell_includes_outbound_directional_glyph() {
        let kind = PeerOutboundKind::Tell {
            target: "planner".into(),
            body: "FYI: PR #187 is open.".into(),
        };
        let lines = render_outbound(&kind, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2934}'),
            "outbound glyph ⤴ U+2934 must appear in header: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_inbound_question_includes_inbound_directional_glyph() {
        let kind = PeerInboundKind::Question {
            from: "alice".into(),
            org: "org".into(),
            body: "what?".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2935}'),
            "inbound glyph ⤵ U+2935 must appear in header: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_inbound_reply_includes_inbound_directional_glyph() {
        let kind = PeerInboundKind::Reply {
            from: "alice".into(),
            org: "org".into(),
            body: "answer".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2935}'),
            "inbound glyph ⤵ U+2935 must appear in header: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_inbound_delivery_failure_carries_inbound_glyph() {
        let kind = PeerInboundKind::DeliveryFailure {
            target: "planner".into(),
            org: "Personal".into(),
            reason: "target spawn failed".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2935}'),
            "DeliveryFailure routes through render_inbound and carries ⤵: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_inbound_suppress_header_drops_directional_glyph_too() {
        let kind = PeerInboundKind::Message {
            from: "implementer".into(),
            org: "Personal".into(),
            body: "PR ready.".into(),
        };
        let lines = render_inbound(&kind, true, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            !s.iter().any(|line| line.contains('\u{2935}')),
            "suppress_header drops the whole header (including glyph): {s:?}"
        );
    }

    #[test]
    fn render_worker_spawn_failed_uses_distinct_chrome() {
        let kind = PeerInboundKind::WorkerSpawnFailed {
            label: "planner".into(),
            reason: "git not found".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        // Uses ✗ rather than ▶ to mark it as a system notice, not a peer row.
        assert!(s[0].contains("\u{2717}"), "✗ glyph: {:?}", s[0]);
        assert!(s[0].contains("Worker 'planner' spawn failed"));
        assert!(s.last().unwrap().contains("git not found"));
    }

    #[test]
    fn detect_gotify_inbound_parses_fields() {
        let text = "[Gotify - app 'Backups', priority 3]\nNightly backup\nAll volumes done";
        match detect_inbound(text).expect("gotify") {
            PeerInboundKind::Gotify { app, title, message, priority } => {
                assert_eq!(app, "Backups");
                assert_eq!(title, "Nightly backup");
                assert_eq!(message, "All volumes done");
                assert_eq!(priority, 3);
            }
            other => panic!("expected Gotify, got {other:?}"),
        }
    }

    #[test]
    fn detect_gotify_inbound_preserves_multiline_message() {
        let text = "[Gotify - app 'CI', priority 8]\nBuild failed\nstep 1 ok\nstep 2 failed";
        match detect_inbound(text).expect("gotify") {
            PeerInboundKind::Gotify { title, message, priority, .. } => {
                assert_eq!(title, "Build failed");
                assert_eq!(message, "step 1 ok\nstep 2 failed");
                assert_eq!(priority, 8);
            }
            other => panic!("expected Gotify, got {other:?}"),
        }
    }

    #[test]
    fn detect_gotify_inbound_rejects_non_numeric_priority() {
        // A malformed priority falls through to plain-text rendering
        // rather than mis-parsing.
        assert!(detect_inbound("[Gotify - app 'X', priority high]\nt\nm").is_none());
    }

    #[test]
    fn gotify_has_no_peer_sender_identity() {
        // Grouping keys off peer_sender_identity; Gotify returns None so
        // it never merges into a peer messaging group.
        let kind = PeerInboundKind::Gotify {
            app: "Backups".into(),
            title: "t".into(),
            message: "m".into(),
            priority: 5,
        };
        assert_eq!(kind.peer_sender_identity(), None);
    }

    #[test]
    fn detect_cron_inbound_parses_prompt_body() {
        let text = "[Cron]\n\nrun the morning summary";
        match detect_inbound(text).expect("cron") {
            PeerInboundKind::Cron { prompt } => assert_eq!(prompt, "run the morning summary"),
            other => panic!("expected Cron, got {other:?}"),
        }
    }

    #[test]
    fn detect_cron_inbound_preserves_multiline_prompt() {
        let text = "[Cron]\n\npost the EOD report\ninclude the realized PnL";
        match detect_inbound(text).expect("cron") {
            PeerInboundKind::Cron { prompt } => {
                assert_eq!(prompt, "post the EOD report\ninclude the realized PnL");
            }
            other => panic!("expected Cron, got {other:?}"),
        }
    }

    #[test]
    fn render_cron_inbound_uses_cron_glyph_and_label() {
        let kind = PeerInboundKind::Cron { prompt: "deploy the release".into() };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains('\u{25f4}'), "◴ cron glyph: {:?}", s[0]);
        assert!(s[0].contains("Cron"), "Cron label: {:?}", s[0]);
        assert!(s.last().unwrap().contains("deploy the release"), "prompt body: {:?}", s.last());
    }

    #[test]
    fn cron_has_no_peer_sender_identity() {
        // Grouping keys off peer_sender_identity; Cron returns None so it
        // never merges into a peer messaging group (mirrors Gotify).
        let kind = PeerInboundKind::Cron { prompt: "x".into() };
        assert_eq!(kind.peer_sender_identity(), None);
    }

    #[test]
    fn render_gotify_notification_full_shape() {
        let kind = PeerInboundKind::Gotify {
            app: "Backups".into(),
            title: "Nightly backup".into(),
            message: "All volumes done".into(),
            priority: 3,
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, false));
        assert!(s[0].contains('\u{25C8}'), "gotify glyph in header: {:?}", s[0]);
        assert!(s[0].contains("app 'Backups'"), "app in header: {:?}", s[0]);
        assert!(s[0].contains("priority 3"), "priority in header: {:?}", s[0]);
        assert!(!s[0].contains('\u{25B6}'), "no peer row glyph: {:?}", s[0]);
        assert!(s.iter().any(|l| l.contains("Nightly backup")), "title in body: {s:?}");
        assert!(s.iter().any(|l| l.contains("All volumes done")), "message in body: {s:?}");
    }

    #[test]
    fn render_gotify_high_priority_uses_warning_color() {
        let kind = PeerInboundKind::Gotify {
            app: "Sec".into(),
            title: "Alert".into(),
            message: "bad".into(),
            priority: 8,
        };
        let lines = render_inbound(&kind, false, false);
        let priority_span =
            lines[0].spans.iter().find(|sp| sp.content.contains('8')).expect("priority span");
        assert_eq!(priority_span.style.fg, Some(theme::STATUS_WARNING));
    }

    #[test]
    fn render_gotify_low_priority_uses_dim_color() {
        let kind = PeerInboundKind::Gotify {
            app: "Backups".into(),
            title: "ok".into(),
            message: "done".into(),
            priority: 2,
        };
        let lines = render_inbound(&kind, false, false);
        let priority_span =
            lines[0].spans.iter().find(|sp| sp.content.contains('2')).expect("priority span");
        assert_eq!(priority_span.style.fg, Some(theme::DIM));
    }

    #[test]
    fn render_gotify_collapsed_shows_title_summary_only() {
        let kind = PeerInboundKind::Gotify {
            app: "Backups".into(),
            title: "Nightly backup".into(),
            message: "All volumes done".into(),
            priority: 3,
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, true));
        assert!(s[0].contains('\u{25C8}'), "header still renders when collapsed: {:?}", s[0]);
        assert!(s.iter().any(|l| l.contains("Nightly backup")), "title in summary: {s:?}");
        assert!(s.iter().any(|l| l.contains("click or ctrl+x to expand")));
        assert!(
            !s.iter().any(|l| l.contains("All volumes done")),
            "message hidden collapsed: {s:?}"
        );
    }

    fn make_tc(sdk_tool_name: &str, raw_input: serde_json::Value) -> crate::app::ToolCallInfo {
        crate::app::ToolCallInfo {
            id: "tc-1".into(),
            title: "tc-1".into(),
            sdk_tool_name: sdk_tool_name.into(),
            raw_input: Some(raw_input),
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: crate::agent::model::ToolCallStatus::InProgress,
            content: vec![],
            hidden: false,
            terminal_id: None,
            terminal_command: None,
            terminal_output: None,
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: crate::app::TerminalSnapshotMode::AppendOnly,
            monitor_output_tail: Vec::default(),
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: crate::app::BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    #[test]
    fn detect_outbound_recognises_peers_ask_with_target_arg() {
        let tc = make_tc(
            "mcp__forge__peers__ask_agent",
            serde_json::json!({ "target": "gateway-backend", "prompt": "?" }),
        );
        match detect_outbound(&tc) {
            Some(PeerOutboundKind::Ask { target, body }) => {
                assert_eq!(target, "gateway-backend");
                assert_eq!(body, "?");
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn detect_outbound_recognises_workers_ask_with_label_arg() {
        let tc = make_tc(
            "mcp__forge__workers__ask",
            serde_json::json!({ "label": "planner", "question": "ready?" }),
        );
        match detect_outbound(&tc) {
            Some(PeerOutboundKind::Ask { target, body }) => {
                assert_eq!(target, "planner");
                assert_eq!(body, "ready?");
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn detect_outbound_recognises_workers_tell_with_label_arg() {
        let tc = make_tc(
            "mcp__forge__workers__tell",
            serde_json::json!({ "label": "implementer", "message": "PR #199 ready" }),
        );
        match detect_outbound(&tc) {
            Some(PeerOutboundKind::Tell { target, body }) => {
                assert_eq!(target, "implementer");
                assert_eq!(body, "PR #199 ready");
            }
            other => panic!("expected Tell, got {other:?}"),
        }
    }

    #[test]
    fn detect_outbound_ignores_workers_spawn_and_list() {
        let spawn = make_tc(
            "mcp__forge__workers__spawn",
            serde_json::json!({ "label": "planner", "charter": "..." }),
        );
        assert!(detect_outbound(&spawn).is_none(), "spawn falls through to standard tool card");

        let list = make_tc("mcp__forge__workers__list", serde_json::json!({}));
        assert!(detect_outbound(&list).is_none(), "list falls through to standard tool card");
    }

    #[test]
    fn detect_outbound_ignores_other_tools() {
        let tc = make_tc("Bash", serde_json::json!({ "command": "ls" }));
        assert!(detect_outbound(&tc).is_none());
    }
}
