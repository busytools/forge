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
//!    `Reply`, `DeliveryFailure`, `WorkerSpawnFailed`) plus the `Gotify`,
//!    `Cron` and `Slack` blocks, which render with their own chrome (glyph
//!    + source label).
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
//! Visual reference: `docs/book/src/ui/peers.md`.

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
#[derive(Debug)]
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
    /// `[Slack - workspace 'X', <channel>] id ... ts ...\n<author>: <text>` -
    /// a matched Slack message delivered as a user turn. Rendered with the
    /// ◇ glyph + a `Slack` source label so it reads as an external event,
    /// not agent traffic. Never groups with peer envelopes (see
    /// [`PeerInboundKind::peer_sender_identity`]).
    Slack {
        workspace: String,
        channel: String,
        /// `None` when there is no name to print: a bot message arrives with
        /// `user: None`, which the producer writes as `unknown`, and
        /// `message.user` is otherwise a raw id until the producer resolves a
        /// handle.
        author: Option<String>,
        body: String,
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
            Self::WorkerSpawnFailed { .. }
            | Self::Gotify { .. }
            | Self::Cron { .. }
            | Self::Slack { .. } => "",
        }
    }

    /// The peer/worker sender identity used for envelope grouping (the
    /// `from` / `target` / `label` name). `None` for `Gotify`, `Cron` and
    /// `Slack`: those are not agent traffic and must never merge into a peer
    /// messaging group - the grouping predicates key off `Some(..)` here.
    pub(crate) fn peer_sender_identity(&self) -> Option<&str> {
        match self {
            Self::Question { from, .. } | Self::Message { from, .. } | Self::Reply { from, .. } => {
                Some(from)
            }
            Self::DeliveryFailure { target, .. } => Some(target),
            Self::WorkerSpawnFailed { label, .. } => Some(label),
            Self::Gotify { .. } | Self::Cron { .. } | Self::Slack { .. } => None,
        }
    }
}

/// One outbound peer or worker block parsed from a `mcp__forge__peers__*`
/// or `mcp__forge__workers__ask|tell` tool_use card. The redesigned
/// chrome drops the family / correlation_id chrome - both peer and
/// worker calls render with the same `▶ Verb name` shape.
#[derive(Debug)]
pub(crate) enum PeerOutboundKind {
    Ask { target: String, body: String },
    Tell { target: String, body: String },
}

/// Modifier suffix surfaced inline after the `Verb name` header when
/// the envelope is a notice variant. Renders as ` - ⚠ <label>` in
/// `STATUS_WARNING`.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// The envelope's own correlation id out of a peer wrapper header
/// (`[Message id=t-1a2b3c from agent ...]`). Unique per delivered
/// envelope, so it keys an inbound-led messaging group independently
/// of where the message sits in the session - unlike a positional key,
/// it survives history pruning and index shifts.
pub(crate) fn inbound_envelope_id(text: &str) -> Option<&str> {
    // split_once, not split: an unterminated bracket has no header at
    // all, and `detect_inbound` rejects it for the same reason.
    let (header, _) = text.strip_prefix('[')?.split_once(']')?;
    // Anchored on the leading space: the worker-spawn-failure header
    // puts a caller-supplied label before the id, so a label containing
    // `id=` would otherwise win the match.
    let after = header.split_once(" id=")?.1;
    let id = &after[..after.find([' ', ':']).unwrap_or(after.len())];
    (!id.is_empty()).then_some(id)
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

    if let Some(rest) = header.strip_prefix("Slack - workspace '") {
        let (workspace, channel) = take_until(rest, "', ")?;
        // The `id … ts …` tail (and ` in thread …`) sits past the closing
        // bracket, so the body is what follows the newline.
        let (_, rest) = after_bracket.split_once('\n')?;
        // `slack_message_to_prose` always writes `<author>: <text>`, so a body
        // without the separator is not this shape at all.
        let (author, body) = rest.split_once(": ")?;
        // A raw Slack id is not a name to print: `message.user` reaches the
        // prose as a `U…` id, and a bot post as the literal `unknown`.
        let author =
            if author == "unknown" || is_slack_id(author) { None } else { Some(author.to_owned()) };
        return Some(PeerInboundKind::Slack {
            workspace: workspace.to_owned(),
            channel: channel.to_owned(),
            author,
            body: body.to_owned(),
        });
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
#[cfg(test)]
pub(crate) fn render_inbound(
    kind: &PeerInboundKind,
    suppress_header: bool,
    collapsed: bool,
) -> Vec<Line<'static>> {
    let mut copy_rows = Vec::new();
    render_inbound_with_metas(kind, suppress_header, collapsed, &mut copy_rows)
}

/// As [`Self::render_inbound`], also emitting each row's copy provenance:
/// the header and collapsed-summary rows are chrome, the tree body rows
/// carry the 5-column connector prefix as chrome.
pub(crate) fn render_inbound_with_metas(
    kind: &PeerInboundKind,
    suppress_header: bool,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    match kind {
        PeerInboundKind::Question { from, body, .. } => render_block(
            "Question",
            from,
            None,
            body,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::Message { from, body, .. } => render_block(
            "Message",
            from,
            None,
            body,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::Reply { from, body, .. } => render_block(
            "Reply",
            from,
            None,
            body,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::DeliveryFailure { target, reason, .. } => render_block(
            "Ask",
            target,
            Some(NoticeModifier::Undeliverable),
            reason,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::WorkerSpawnFailed { label, reason } => {
            render_worker_spawn_failed(label, reason, copy_rows)
        }
        PeerInboundKind::Gotify { app, title, message, priority } => render_gotify_notification(
            app,
            *priority,
            title,
            message,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::Cron { prompt } => {
            render_cron_prompt(prompt, suppress_header, collapsed, copy_rows)
        }
        PeerInboundKind::Slack { workspace, channel, author, body } => render_slack_notification(
            workspace,
            channel,
            author.as_deref(),
            body,
            suppress_header,
            collapsed,
            copy_rows,
        ),
    }
}

/// Build the styled lines for an outbound peer / worker block.
#[cfg(test)]
pub(crate) fn render_outbound(kind: &PeerOutboundKind, collapsed: bool) -> Vec<Line<'static>> {
    let mut copy_rows = Vec::new();
    render_outbound_with_metas(kind, collapsed, &mut copy_rows)
}

/// As [`Self::render_outbound`], also emitting each row's copy provenance.
pub(crate) fn render_outbound_with_metas(
    kind: &PeerOutboundKind,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    match kind {
        PeerOutboundKind::Ask { target, body } => {
            render_block("Ask", target, None, body, OUTBOUND_GLYPH, false, collapsed, copy_rows)
        }
        PeerOutboundKind::Tell { target, body } => {
            render_block("Tell", target, None, body, OUTBOUND_GLYPH, false, collapsed, copy_rows)
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

/// Kind-icon for an inbound Slack message. U+25C7 WHITE DIAMOND - already the
/// Task / Agent kind row's glyph, and the hollow companion to gotify's filled
/// ◈, distinct from cron's ◴. Monochrome + width-1, matching the codebase
/// glyph convention.
const SLACK_GLYPH: &str = "\u{25c7}";

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
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
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
        copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    }
    if collapsed {
        push_collapsed_summary(&mut lines, copy_rows, body);
    } else {
        push_tree_body_lines(&mut lines, copy_rows, body);
    }
    lines
}

/// One-off renderer for the `WorkerSpawnFailed` lifecycle notice.
/// Kept distinct from `render_block` because the spawn failure is a
/// workspace-generated system event, not a peer comm; carrying it
/// through the verb-row shape would force a non-fitting verb. The
/// ✗-glyph + plain prose treatment signals "system notice, not a
/// peer row" at a glance.
fn render_worker_spawn_failed(
    label: &str,
    reason: &str,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
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
    copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    push_tree_body_lines(&mut lines, copy_rows, reason);
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
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
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
        copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    }
    let body = if message.is_empty() { title.to_owned() } else { format!("{title}\n{message}") };
    if collapsed {
        push_collapsed_summary(&mut lines, copy_rows, &body);
    } else {
        push_tree_body_lines(&mut lines, copy_rows, &body);
    }
    lines
}

/// Render a fired-cron block. Distinct chrome from peer rows: the ◴ cron
/// glyph + a `Cron` source label (the SCHEDULES-section cron glyph), body =
/// the fired prompt under the standard tree connectors. `suppress_header`
/// drops the header line (streak follower); cron turns stand alone in
/// practice, so it's normally rendered in full.
fn render_cron_prompt(
    prompt: &str,
    suppress_header: bool,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
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
        copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    }
    if collapsed {
        push_collapsed_summary(&mut lines, copy_rows, prompt);
    } else {
        push_tree_body_lines(&mut lines, copy_rows, prompt);
    }
    lines
}

/// Render an inbound Slack message block. Distinct chrome from the peer and
/// Gotify rows: the ◇ glyph and one header row carrying the channel, then the
/// workspace and author, over the message body. The author clause needs a
/// resolved handle, so it is dropped for a bot post (`unknown`) and for a raw
/// id alike.
fn render_slack_notification(
    workspace: &str,
    channel: &str,
    author: Option<&str>,
    body: &str,
    suppress_header: bool,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !suppress_header {
        let dim = Style::default().fg(theme::DIM);
        let mut header = Line::default();
        header.spans.push(Span::raw("  "));
        header.spans.push(Span::styled(
            SLACK_GLYPH.to_owned(),
            Style::default().fg(theme::SLACK).add_modifier(Modifier::BOLD),
        ));
        header.spans.push(Span::raw(" "));
        // An id is never dressed as a channel: a DM's label is the partner's
        // user id, and `#U0AE0CBJ77G` reads as a channel that does not exist.
        let label = if is_slack_id(channel) { channel.to_owned() } else { format!("#{channel}") };
        header.spans.push(Span::styled(label, Style::default().add_modifier(Modifier::BOLD)));
        header.spans.push(Span::styled(format!(" \u{b7} {workspace}"), dim));
        if let Some(author) = author {
            header.spans.push(Span::styled(format!(" \u{b7} {author}"), dim));
        }
        lines.push(header);
        copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    }
    let body = tidy_mrkdwn(body);
    if collapsed {
        push_collapsed_summary(&mut lines, copy_rows, &body);
    } else {
        push_tree_body_lines(&mut lines, copy_rows, &body);
    }
    lines
}

/// Strip Slack's mrkdwn down to plain text: emphasis markers drop, a labelled
/// link becomes `label: url`, and the `&`/`<`/`>` entities decode.
fn tidy_mrkdwn(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&strip_emphasis(&rest[..open]));
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            out.push_str(&strip_emphasis(&rest[open..]));
            return decode_entities(&out);
        };
        // The target goes out whole: Slack reads no emphasis inside a URL.
        if let Some((url, label)) = after[..close].split_once('|') {
            out.push_str(&strip_emphasis(label));
            out.push_str(": ");
            out.push_str(url);
        } else {
            out.push('<');
            out.push_str(&after[..close]);
            out.push('>');
        }
        rest = &after[close + 1..];
    }
    out.push_str(&strip_emphasis(rest));
    decode_entities(&out)
}

/// Drop the markers around an emphasised span. A marker pair is only emphasis
/// when each end sits at a word boundary with a single marker, so the `_` in
/// an identifier, an unpaired `_` and the `**` of a glob all stay literal.
fn strip_emphasis(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut dropped = vec![false; chars.len()];
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '*' | '_')
            && opens_a_span(&chars, i)
            && let Some(close) = first_closer(&chars, i)
        {
            dropped[i] = true;
            dropped[close] = true;
            i = close + 1;
            continue;
        }
        i += 1;
    }
    chars.iter().zip(&dropped).filter(|(_, dropped)| !**dropped).map(|(c, _)| *c).collect()
}

/// A marker opens a span only at a word boundary, with text after it and no
/// doubled marker on either side - the `_` inside an identifier and the `**`
/// of a glob are both literals rather than openers.
fn opens_a_span(chars: &[char], i: usize) -> bool {
    let marker = chars[i];
    let before = i.checked_sub(1).and_then(|p| chars.get(p));
    let after = chars.get(i + 1).copied();
    before.is_none_or(|p| !p.is_alphanumeric() && *p != marker)
        && after.is_some_and(|n| !n.is_whitespace() && n != marker)
}

/// The first index able to close a span opened at `open`: the same marker,
/// with non-space before it, no word character after it, and no ability to open
/// a span of its own - otherwise the two `*` of a pair of globs would close a
/// span between them.
fn first_closer(chars: &[char], open: usize) -> Option<usize> {
    let marker = chars[open];
    ((open + 1)..chars.len()).find(|&j| {
        chars[j] == marker
            && chars.get(j - 1).is_some_and(|p| !p.is_whitespace())
            && chars.get(j + 1).is_none_or(|n| !n.is_alphanumeric())
            && !opens_a_span(chars, j)
    })
}

/// Slack escapes `&`, `<` and `>` in message text. The angle-bracket entities
/// decode first so a doubly-escaped `&amp;lt;` stays the literal it was.
fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Tree row data for one envelope: the direction glyph, the kind label,
/// and whether the kind is a failure (styled as a warning).
///
/// The KIND is the envelope kind, not the direction - a per-message group
/// is always single-direction, so direction would never discriminate.
pub(crate) fn inbound_kind_row(
    kind: &PeerInboundKind,
) -> Option<(&'static str, &'static str, bool)> {
    let row = match kind {
        PeerInboundKind::Message { .. } => (INBOUND_GLYPH, "message", false),
        PeerInboundKind::Question { .. } => (INBOUND_GLYPH, "question", false),
        PeerInboundKind::Reply { .. } => (INBOUND_GLYPH, "reply", false),
        PeerInboundKind::DeliveryFailure { .. } => (INBOUND_GLYPH, "failed", true),
        PeerInboundKind::WorkerSpawnFailed { .. } => (INBOUND_GLYPH, "spawn failed", true),
        // External events, never agent traffic - excluded from grouping.
        PeerInboundKind::Gotify { .. }
        | PeerInboundKind::Cron { .. }
        | PeerInboundKind::Slack { .. } => return None,
    };
    Some(row)
}

/// Sibling of [`inbound_kind_row`] for outbound calls.
pub(crate) fn outbound_kind_row(kind: &PeerOutboundKind) -> (&'static str, &'static str) {
    match kind {
        PeerOutboundKind::Ask { .. } => (OUTBOUND_GLYPH, "ask"),
        PeerOutboundKind::Tell { .. } => (OUTBOUND_GLYPH, "tell"),
    }
}

/// The body text a leaf row previews, per envelope kind. The external-event
/// arms - `Gotify`, `Cron` and `Slack` - are unreachable: `inbound_kind_row`
/// returns `None` for all three, so none ever becomes a leaf.
pub(crate) fn inbound_body(kind: &PeerInboundKind) -> &str {
    match kind {
        PeerInboundKind::Message { body, .. }
        | PeerInboundKind::Question { body, .. }
        | PeerInboundKind::Reply { body, .. }
        | PeerInboundKind::Slack { body, .. } => body,
        PeerInboundKind::DeliveryFailure { reason, .. }
        | PeerInboundKind::WorkerSpawnFailed { reason, .. } => reason,
        PeerInboundKind::Gotify { message, .. } => message,
        PeerInboundKind::Cron { prompt } => prompt,
    }
}

/// A leaf row's content: `<peer> · <first non-blank body line>`. The
/// renderer clips this to a computed budget, so no fixed length here -
/// end-ellipsis keeps the peer name, which is at the head.
pub(crate) fn kind_row_target(peer: &str, body: &str) -> String {
    let head = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if head.is_empty() { peer.to_owned() } else { format!("{peer} \u{b7} {head}") }
}

/// One-line collapsed summary shape: `  └─ <first line of body, truncated>  click or ctrl+x to expand`.
/// Skips entirely when the body is empty so notice variants (which
/// have no prose body) don't render an orphan `└─ click to expand`
/// row pointing at nothing.
fn push_collapsed_summary(
    lines: &mut Vec<Line<'static>>,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
    body: &str,
) {
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
    copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
}

/// Push the body lines under `│  ` / `└─ ` tree connectors - matches
/// `tool_call::standard::render_tool_content`'s pipe / corner glyph
/// pair. Renders the FULL body when expanded - no truncation; the
/// collapsed summary (see [`push_collapsed_summary`]) is the only
/// place we truncate, and only to fit a single summary row. When the
/// body is empty, pushes nothing so the header stands alone.
fn push_tree_body_lines(
    lines: &mut Vec<Line<'static>>,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
    body: &str,
) {
    /// Display columns of the `  │  ` / `  └─ ` connector prefix.
    const CONNECTOR_COLS: u16 = 5;
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
        copy_rows.push(crate::ui::copy::CopyRowMeta::hard_line().offset_chrome(CONNECTOR_COLS));
    }
}

// ---------- parsing helpers ----------

/// True for a raw Slack id (`C0C0T5E6RM1`, `U9ABCD`) rather than a name: an
/// uppercase first character followed by uppercase letters and digits only.
/// Both `message.user` and a DM's conversation label reach the prose as one
/// until the producer resolves a handle. The shape is a heuristic, not a
/// guarantee: an all-caps channel name matches it and loses its `#`.
fn is_slack_id(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|first| first.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

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

/// Render the L2 summary TREE for a messaging group: a parent count row
/// plus one kind row per envelope kind and one leaf per message, drawn
/// by the same renderer the tool groups use.
pub(crate) fn render_messaging_group_summary_line(
    segment: &crate::ui::message::grouping::MessagingGroupSegment,
    spinner_glyph: char,
    max_width: usize,
) -> Vec<Line<'static>> {
    crate::ui::tool_call::render_group_summary_line(
        &segment.summary,
        segment.aggregate_status,
        spinner_glyph,
        max_width,
        None,
        &crate::ui::tool_call::SummaryChrome::MESSAGING,
    )
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

    /// Every real header shape, because the id keys the messaging
    /// group's collapse level. `DeliveryFailure` and
    /// `WorkerSpawnFailed` terminate the id with `:` rather than a
    /// space, so both terminators have to be handled.
    ///
    /// The `None` rows matter as much: the caller falls back to a
    /// positional key when this returns `None`, which is exactly the
    /// collision the envelope id exists to avoid.
    #[test]
    fn inbound_envelope_id_covers_every_header_shape() {
        let cases: &[(&str, Option<&str>)] = &[
            ("[Question id=q-1a2b from agent 'lead' (org 'forge')]\n\nbody", Some("q-1a2b")),
            ("[Message id=t-1a2b from agent 'lead' (org 'forge')]\n\nbody", Some("t-1a2b")),
            ("[Reply id=t-9f8e from agent 'lead' (org 'forge')]\n\nbody", Some("t-9f8e")),
            ("[Ask id=q-77 to agent 'x' (org 'forge') failed to deliver: gone]\n\n", Some("q-77")),
            ("[Worker 'runner' spawn failed id=w-5: boom]", Some("w-5")),
            // The label is caller-supplied and precedes the id, so the
            // `id=` anchor has to be the space-prefixed one or a label
            // containing `id=` wins the match.
            ("[Worker 'grid=a' spawn failed id=w-5: boom]", Some("w-5")),
            // No id in the header at all.
            ("[Gotify - app 'ci', priority 5]\ntitle\nbody", None),
            ("[Cron]\n\nprompt", None),
            // Not an envelope.
            ("plain user text", None),
            ("[unterminated id=t-1", None),
            // Present but empty - must not key a group on "".
            ("[Message id= from agent 'lead' (org 'forge')]\n\nbody", None),
        ];
        for (text, want) in cases {
            assert_eq!(inbound_envelope_id(text), *want, "for {text:?}");
        }
    }

    #[test]
    fn detect_inbound_rejects_non_peer_text() {
        assert!(detect_inbound("plain user message").is_none());
        assert!(detect_inbound("[not-a-peer-prefix]").is_none());
        assert!(detect_inbound("[Question id=q-bad").is_none());
        // The Slack header alone is not an envelope: the body line is what
        // carries the author and the message.
        assert!(
            detect_inbound("[Slack - workspace 'W', chan] id C1 ts 1.2").is_none(),
            "a Slack header with no body line is not an envelope",
        );
        assert!(
            detect_inbound("[Slack - workspace 'W', chan] id C1 ts 1.2\nno author prefix")
                .is_none(),
            "nor is a Slack body with no `<author>: ` separator",
        );
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

    /// The shipped prose exactly as `slack_message_to_prose` emits it: the
    /// bracketed part carries the workspace and conversation, the line under
    /// it is `<author>: <text>`, and a bot message arrives with no author at
    /// all - which reaches the prose as the literal `unknown`.
    #[test]
    fn detect_slack_inbound_parses_header_and_drops_the_unknown_author() {
        let text = "[Slack - workspace 'Trust Machines', granite-staging-alerts] id C0AE0CBJ77G ts 1789182982.499299\nunknown: _Large STX Transfer_\nAmount: 233468.293536 STX (~$60434.82 USD)";
        match detect_inbound(text).expect("slack") {
            PeerInboundKind::Slack { workspace, channel, author, body } => {
                assert_eq!(workspace, "Trust Machines", "workspace off the header");
                assert_eq!(channel, "granite-staging-alerts", "channel off the header");
                assert_eq!(author, None, "`unknown` is not an author to print");
                assert_eq!(
                    body,
                    "_Large STX Transfer_\nAmount: 233468.293536 STX (~$60434.82 USD)",
                );
            }
            other => panic!("expected Slack, got {other:?}"),
        }
    }

    /// A resolved handle is kept, lowercase or title-cased. `alert-bot` is the
    /// shape a handle takes; today's producer emits raw ids, which the id case
    /// below covers.
    #[test]
    fn detect_slack_inbound_keeps_a_real_author() {
        let text = "[Slack - workspace 'Subspace', mainnet-chain-alerts] id C0AE1 ts 1789.5\nalert-bot: Slow slot: 57296338 took 1s";
        match detect_inbound(text).expect("slack") {
            PeerInboundKind::Slack { workspace, channel, author, body } => {
                assert_eq!(workspace, "Subspace");
                assert_eq!(channel, "mainnet-chain-alerts");
                assert_eq!(author.as_deref(), Some("alert-bot"));
                assert_eq!(body, "Slow slot: 57296338 took 1s");
            }
            other => panic!("expected Slack, got {other:?}"),
        }

        // Only an id is dropped: an uppercase first letter alone is a name.
        let titled = "[Slack - workspace 'Subspace', mainnet-chain-alerts] id C0AE1 ts 1789.5\nGranite-Bot: Slow slot: 57296338 took 1s";
        match detect_inbound(titled).expect("slack") {
            PeerInboundKind::Slack { author, .. } => {
                assert_eq!(
                    author.as_deref(),
                    Some("Granite-Bot"),
                    "a title-cased handle names someone"
                );
            }
            other => panic!("expected Slack, got {other:?}"),
        }
    }

    /// The producer appends ` in thread <ts>` after the ts for a threaded
    /// reply, so the detector skips everything past the closing bracket up to
    /// the newline rather than assuming the body starts after the ts.
    #[test]
    fn detect_slack_inbound_drops_the_thread_suffix_with_the_header() {
        let text = "[Slack - workspace 'W', chan] id C1 ts 1789.5 in thread 1789.4\nalert-bot: hi";
        match detect_inbound(text).expect("slack") {
            PeerInboundKind::Slack { channel, author, body, .. } => {
                assert_eq!(channel, "chan");
                assert_eq!(author.as_deref(), Some("alert-bot"));
                assert_eq!(body, "hi");
            }
            other => panic!("expected Slack, got {other:?}"),
        }
    }

    /// A matcher widened to catch Slack must leave the neighbouring external
    /// sources alone: Gotify keeps its own kind, and peer traffic is still a
    /// peer envelope rather than being swallowed as Slack.
    #[test]
    fn slack_detection_leaves_gotify_and_peer_prose_alone() {
        let gotify = "[Gotify - app 'Backups', priority 3]\nNightly backup\nAll volumes done";
        assert!(
            matches!(detect_inbound(gotify), Some(PeerInboundKind::Gotify { .. })),
            "a Gotify notice still detects as Gotify",
        );
        let peer = "[Message id=t-1a2b from agent 'lead' (org 'forge')]\n\nbody";
        assert!(
            matches!(detect_inbound(peer), Some(PeerInboundKind::Message { .. })),
            "a peer wrapper still detects as a peer envelope",
        );
    }

    #[test]
    fn slack_has_no_peer_sender_identity() {
        // Grouping keys off peer_sender_identity; Slack returns None so it
        // never merges into a peer messaging group (mirrors Gotify).
        let kind = PeerInboundKind::Slack {
            workspace: "W".into(),
            channel: "c".into(),
            author: None,
            body: "x".into(),
        };
        assert_eq!(kind.peer_sender_identity(), None);
    }

    /// The block's shape: one header row carrying the glyph, the bold channel,
    /// then the dim workspace and author, with the body under the standard
    /// tree.
    ///
    /// ```text
    /// Slack
    ///   ◇ #granite-staging-alerts · Trust Machines · granite-bot
    ///   └─ Large STX Transfer: 233468.293536 STX (~$60434.82 USD)
    /// ```
    ///
    /// `granite-bot` is a resolved handle, which today's producer does not
    /// emit: `message.user` arrives as a raw id and the block drops it.
    #[test]
    fn render_slack_notification_header_carries_the_channel_workspace_and_author() {
        let kind = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "granite-staging-alerts".into(),
            author: Some("granite-bot".into()),
            body: "Large STX Transfer: 233468.293536 STX (~$60434.82 USD)".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains('\u{25C7}'), "◇ slack glyph in header: {:?}", s[0]);
        assert!(s[0].contains("#granite-staging-alerts"), "channel in header: {:?}", s[0]);
        assert!(s[0].contains("Trust Machines"), "workspace in header: {:?}", s[0]);
        assert!(s[0].contains("granite-bot"), "author in header: {:?}", s[0]);
        assert!(!s[0].contains('\u{25B6}'), "no peer row glyph: {:?}", s[0]);
        assert!(s.last().unwrap().contains("Large STX Transfer"), "body: {:?}", s.last());
        let channel_span = lines[0]
            .spans
            .iter()
            .find(|sp| sp.content.contains("#granite-staging-alerts"))
            .expect("the channel span");
        assert!(
            channel_span.style.add_modifier.contains(Modifier::BOLD),
            "the channel reads bold on the header",
        );
        let glyph_span =
            lines[0].spans.iter().find(|sp| sp.content.contains('\u{25C7}')).expect("glyph span");
        assert_eq!(
            glyph_span.style.fg,
            Some(theme::SLACK),
            "the ◇ glyph wears the Slack accent, not the neighbouring gotify one",
        );
    }

    /// A DM's label is the partner's user id, and an id is never dressed as a
    /// channel: `#U0AE0CBJ77G` reads as a channel that does not exist. A name
    /// that merely starts uppercase still is one.
    #[test]
    fn render_slack_notification_prefixes_a_name_but_not_an_id() {
        let id_label = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "U0AE0CBJ77G".into(),
            author: None,
            body: "ping".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&id_label, false, false));
        assert!(s[0].contains("U0AE0CBJ77G"), "the label is still named: {:?}", s[0]);
        assert!(!s[0].contains('#'), "and never prefixed as a channel: {:?}", s[0]);

        let name_label = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "General".into(),
            author: None,
            body: "ping".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&name_label, false, false));
        assert!(s[0].contains("#General"), "an uppercase first letter is not an id: {:?}", s[0]);
    }

    /// The collapsed block keeps the header and shows one tidied line of the
    /// body with the expand hint, the same summary every other block collapses
    /// to.
    #[test]
    fn render_slack_collapsed_shows_a_tidied_body_summary() {
        let kind = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "granite-staging-alerts".into(),
            author: None,
            body: "_Large STX Transfer_\nAmount: 233468.293536 STX (~$60434.82 USD)".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, true));
        assert!(s[0].contains('\u{25C7}'), "header still renders when collapsed: {:?}", s[0]);
        assert!(
            s.iter().any(|line| line.contains("Large STX Transfer")),
            "the tidied first body line is the summary: {s:?}",
        );
        assert!(!s.iter().any(|line| line.contains('_')), "markers gone collapsed: {s:?}");
        assert!(
            !s.iter().any(|line| line.contains("Amount:")),
            "the second body line stays hidden: {s:?}",
        );
        assert!(s.iter().any(|line| line.contains("click or ctrl+x to expand")));
    }

    /// A raw user id is not a name, so production drops the author clause on
    /// every real delivery until the producer resolves a handle.
    #[test]
    fn detect_slack_inbound_drops_an_id_shaped_author() {
        let text = "[Slack - workspace 'Trust Machines', granite-staging-alerts] id C0AE ts 1789.5\nU0AE0CBJ77G: Large STX Transfer";
        match detect_inbound(text).expect("slack") {
            PeerInboundKind::Slack { author, body, .. } => {
                assert_eq!(author, None, "a `U…` id names nobody to print");
                assert_eq!(body, "Large STX Transfer");
            }
            other => panic!("expected Slack, got {other:?}"),
        }
    }

    /// A bot message has no author, and the header drops the whole clause
    /// rather than printing `unknown` at the reader.
    #[test]
    fn render_slack_notification_omits_an_absent_author() {
        let kind = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "granite-staging-alerts".into(),
            author: None,
            body: "Large STX Transfer".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, false));
        assert!(s[0].contains("#granite-staging-alerts"), "channel in header: {:?}", s[0]);
        assert!(s[0].contains("Trust Machines"), "workspace in header: {:?}", s[0]);
        assert_eq!(
            s[0].matches('\u{b7}').count(),
            1,
            "only the channel/workspace separator: {:?}",
            s[0]
        );
    }

    /// The block shows the tidied body, so the markers never reach the reader.
    #[test]
    fn render_slack_notification_tidies_the_body() {
        let kind = PeerInboundKind::Slack {
            workspace: "W".into(),
            channel: "c".into(),
            author: None,
            body: "_Large STX Transfer_".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, false));
        assert!(s.last().unwrap().contains("Large STX Transfer"), "body: {:?}", s.last());
        assert!(!s.iter().any(|l| l.contains('_')), "no markers survive: {s:?}");
    }

    #[test]
    fn tidy_mrkdwn_strips_emphasis_markers() {
        assert_eq!(tidy_mrkdwn("_Large STX Transfer_"), "Large STX Transfer");
        assert_eq!(tidy_mrkdwn("*build failed*"), "build failed");
    }

    #[test]
    fn tidy_mrkdwn_turns_a_labelled_link_into_label_colon_url() {
        assert_eq!(
            tidy_mrkdwn("View <https://explorer.hiro.so/txid/0x37599daf|Transaction>"),
            "View Transaction: https://explorer.hiro.so/txid/0x37599daf",
        );
    }

    #[test]
    fn tidy_mrkdwn_leaves_a_plain_string_untouched() {
        let plain = "Amount: 233468.293536 STX (~$60434.82 USD)";
        assert_eq!(tidy_mrkdwn(plain), plain);
    }

    #[test]
    fn tidy_mrkdwn_decodes_slack_entities() {
        assert_eq!(tidy_mrkdwn("AT&amp;T &lt;ok&gt;"), "AT&T <ok>");
        // A doubly-escaped `&amp;lt;` was a literal `<` the author typed, so
        // it must survive as one rather than decoding twice.
        assert_eq!(tidy_mrkdwn("&amp;lt;"), "&lt;");
    }

    /// Marker characters inside a link belong to the link: the URL is emitted
    /// whole rather than having its `_` read as emphasis.
    #[test]
    fn tidy_mrkdwn_leaves_a_link_url_whole() {
        assert_eq!(
            tidy_mrkdwn("<https://example.com/a_b|log_file>"),
            "log_file: https://example.com/a_b",
        );
    }

    /// A marker is emphasis only at a word boundary with a single marker at
    /// each end. An identifier, an unpaired marker and a doubled one are all
    /// literal - alert text and file paths are full of every shape.
    #[test]
    fn tidy_mrkdwn_leaves_a_marker_that_is_not_emphasis() {
        assert_eq!(tidy_mrkdwn("set some_var_name here"), "set some_var_name here");
        assert_eq!(tidy_mrkdwn("the _bold_suffix here"), "the _bold_suffix here");
        assert_eq!(tidy_mrkdwn("**bold**"), "**bold**");
        assert_eq!(tidy_mrkdwn("__init__"), "__init__");
        assert_eq!(tidy_mrkdwn("src/**/*.rs"), "src/**/*.rs");
        assert_eq!(tidy_mrkdwn("src/*.rs and tests/*.rs"), "src/*.rs and tests/*.rs");
    }

    /// Only a labelled span is a link: a mention token and a bare URL carry no
    /// `|`, and an unmatched `<` has no `>` to close it, so all three go out
    /// as they came.
    #[test]
    fn tidy_mrkdwn_leaves_an_angle_span_that_is_not_a_link() {
        assert_eq!(tidy_mrkdwn("ping <@U123> at <https://x>"), "ping <@U123> at <https://x>");
        assert_eq!(tidy_mrkdwn("a < b"), "a < b");
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
            terminal_output: None,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
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

    fn segment_of(
        blocks: &[crate::app::MessageBlock],
    ) -> crate::ui::message::grouping::MessagingGroupSegment {
        crate::ui::message::grouping::partition_blocks_into_render_units(blocks)
            .into_iter()
            .find_map(|u| match u {
                crate::ui::message::grouping::RenderUnit::MessagingGroup { segment, .. } => {
                    Some(segment)
                }
                _ => None,
            })
            .expect("a messaging group")
    }

    fn inbound_block(kind: &str, from: &str, body: &str) -> crate::app::MessageBlock {
        crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(&format!(
            "[{kind} id=t-{from} from agent '{from}' (org 'forge')]\n\n{body}"
        )))
    }

    /// The bundle summary cycles on click as well as ctrl+x, so it
    /// advertises both.
    #[test]
    fn messaging_group_summary_advertises_click_and_ctrl_x() {
        let blocks = vec![
            inbound_block("Message", "steward", "one"),
            inbound_block("Message", "planner", "two"),
        ];
        let rendered = render_lines_to_strings(&render_messaging_group_summary_line(
            &segment_of(&blocks),
            '\u{280B}',
            80,
        ));
        assert!(rendered[0].ends_with("click or ctrl+x to expand"), "got {rendered:?}");
    }

    /// The parent row is a bare count - every peer appears as a leaf, so
    /// a target clause in the heading would name each one twice.
    #[test]
    fn messaging_group_parent_row_carries_no_target_list() {
        let blocks = vec![
            inbound_block("Message", "steward", "one"),
            inbound_block("Message", "planner", "two"),
        ];
        let rendered = render_lines_to_strings(&render_messaging_group_summary_line(
            &segment_of(&blocks),
            '\u{280B}',
            80,
        ));
        assert!(rendered[0].contains("2 messages"), "got {rendered:?}");
        assert!(!rendered[0].contains("steward"), "no target list on the parent; got {rendered:?}");
        assert!(!rendered[0].contains("inbound from"), "got {rendered:?}");
    }

    /// Always nest, never inline: a kind with one message still gets its
    /// own leaf, so peer names share a column instead of sitting at
    /// ragged widths.
    #[test]
    fn a_single_message_kind_still_nests_its_leaf() {
        let blocks = vec![
            inbound_block("Message", "steward", "one"),
            inbound_block("Reply", "tester", "two"),
        ];
        let rendered = render_lines_to_strings(&render_messaging_group_summary_line(
            &segment_of(&blocks),
            '\u{280B}',
            80,
        ));
        let reply_row = rendered.iter().find(|l| l.contains("reply")).expect("a reply kind row");
        assert!(
            !reply_row.contains("tester"),
            "the peer belongs on its own leaf, not inline on the kind row; got {reply_row:?}",
        );
        assert!(
            rendered.iter().any(|l| l.contains("tester") && !l.contains("reply")),
            "and the leaf exists separately; got {rendered:?}",
        );
    }

    /// Failure kinds carry warning colour on their kind row, and the
    /// leaf keeps the reason - a bare peer name with no reason would
    /// tell the reader nothing about what went wrong.
    #[test]
    fn failure_kind_rows_are_styled_and_keep_their_reason() {
        for (prose, label, reason) in [
            (
                "[Ask id=q-7 to agent 'planner' (org 'forge') failed to deliver: peer asleep]\n\n",
                "failed",
                "peer asleep",
            ),
            (
                "[Worker 'runner' spawn failed id=w-5: worktree busy]",
                "spawn failed",
                "worktree busy",
            ),
        ] {
            let blocks = vec![
                inbound_block("Message", "steward", "one"),
                crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(prose)),
            ];
            let lines = render_messaging_group_summary_line(&segment_of(&blocks), '\u{280B}', 80);
            let kind_span = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content.contains(label))
                .unwrap_or_else(|| panic!("a {label} kind row"));
            assert_eq!(
                kind_span.style.fg,
                Some(theme::STATUS_WARNING),
                "{label} must read as a warning",
            );
            let rendered = render_lines_to_strings(&lines);
            assert!(
                rendered.iter().any(|l| l.contains(reason)),
                "the leaf keeps the reason; got {rendered:?}",
            );
        }
    }

    /// The leaf rows clip to a computed budget, because the outer
    /// layout char-wraps without the tree gutter and an overflowing row
    /// shears the tree. Messaging is the tightest fixture for that, its
    /// expand hint being longer than the tool one.
    #[test]
    fn leaf_rows_fit_a_narrow_width() {
        let blocks = vec![
            inbound_block(
                "Message",
                "steward",
                "a body long enough that it must be clipped to fit",
            ),
            inbound_block("Reply", "planner", "another body that would overflow a narrow terminal"),
        ];
        let rendered = render_lines_to_strings(&render_messaging_group_summary_line(
            &segment_of(&blocks),
            '\u{280B}',
            40,
        ));
        // Row 0 is the parent, which carries the longer messaging hint and
        // can exceed a narrow width - it wraps rather than shearing, since
        // the connectors live on the child rows. The CHILD rows are the
        // shearing risk and must fit.
        for row in rendered.iter().skip(1) {
            assert!(
                unicode_width::UnicodeWidthStr::width(row.as_str()) <= 40,
                "row must fit width 40; got {}: {row:?}",
                unicode_width::UnicodeWidthStr::width(row.as_str()),
            );
        }
        assert!(rendered.iter().any(|l| l.contains("...")), "something clipped; got {rendered:?}");
    }

    /// A delivery failure inside a run must not render under a green
    /// check: the parent status has to learn about inbound failures.
    #[test]
    fn a_delivery_failure_drives_the_parent_status() {
        let blocks = vec![
            inbound_block("Message", "steward", "one"),
            crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(
                "[Ask id=q-7 to agent 'planner' (org 'forge') failed to deliver: gone]\n\n",
            )),
        ];
        assert_eq!(
            segment_of(&blocks).aggregate_status,
            crate::agent::model::ToolCallStatus::Failed,
            "an undelivered message must not report success",
        );
    }
}
