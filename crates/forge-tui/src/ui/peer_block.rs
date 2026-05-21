//! Peer-coordination chat blocks (#114 v1).
//!
//! Two responsibilities:
//!
//! 1. **Inbound detection + rendering**. Pattern-match the bracket-
//!    wrapped prose `forge_workspace::deliver_peer_prompt` injects
//!    into user-turn text (e.g. `[Question id=q-... hop=1/10 from
//!    agent 'forge' (org 'Personal') - reply with tell_agent
//!    in_reply_to=q-...]\n\n<body>`) and render a styled block in
//!    place of the default user-message bubble. Catches all seven
//!    kinds the workspace produces (`Question`, `Message`, `Reply`,
//!    `Late reply`, `Ask ... timed out` (caller-side),
//!    `Ask ... has expired` (recipient-side),
//!    `Ask ... failed to deliver`).
//!
//! 2. **Outbound rendering**. Replace the default tool_use card for
//!    `mcp__forge__peers__ask_agent` / `peers__tell_agent` with a
//!    one-line "→ ask · target · q-id" / "→ tell · target · t-id"
//!    block + a body preview pulled from the tool arguments.
//!
//! Pure rendering — no I/O, no state. Each call parses the text
//! fresh; results aren't cached (text is small, render frames don't
//! call this hot enough to need a cache).
//!
//! Visual reference: `.superpowers/brainstorm/peer-mcp-v1-mockup.html`.

use crate::app::ToolCallInfo;
use crate::ui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// One inbound peer block parsed from the user-turn text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PeerInboundKind {
    Question {
        id: String,
        from: String,
        org: String,
        hop: u8,
        hop_max: u8,
        body: String,
    },
    Message {
        id: String,
        from: String,
        org: String,
        hop: u8,
        hop_max: u8,
        body: String,
    },
    Reply {
        id: String,
        from: String,
        org: String,
        body: String,
    },
    LateReply {
        id: String,
        from: String,
        org: String,
        body: String,
    },
    /// `[Ask id=... to agent 'X' (org 'Y') timed out after 30 minutes - no reply received.]`
    /// Caller-side notice that their own ask hit the 30-min timer.
    CallerTimeout {
        id: String,
        target: String,
        org: String,
        body: String,
    },
    /// `[Ask id=... from agent 'A' (org 'O') has expired - any reply you produce will be tagged late.]`
    /// Recipient-side notice that the caller's ask has been abandoned.
    RecipientExpired {
        id: String,
        from: String,
        org: String,
        body: String,
    },
    /// `[Ask id=... to agent 'X' (org 'Y') failed to deliver: <reason>]`
    /// Caller-side delivery failure (spawn / connection / channel).
    DeliveryFailure {
        id: String,
        target: String,
        org: String,
        reason: String,
    },
}

/// One outbound peer block parsed from a `mcp__forge__peers__*`
/// tool_use card. The correlation id is `None` until the tool call's
/// result arrives.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PeerOutboundKind {
    Ask { target: String, body: String, correlation_id: Option<String> },
    Tell { target: String, body: String, correlation_id: Option<String> },
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
    // variants have an empty body — the bracket itself is the whole
    // message, possibly followed by `\n\n` + extra context. Both
    // are valid.
    let body = after_bracket.strip_prefix("\n\n").unwrap_or("").to_owned();

    if let Some(rest) = header.strip_prefix("Question id=") {
        let (id, rest) = take_until(rest, " hop=")?;
        let (hop_str, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        let (hop, hop_max) = parse_hop(hop_str).unwrap_or((1, 10));
        return Some(PeerInboundKind::Question {
            id: id.to_owned(),
            from,
            org,
            hop,
            hop_max,
            body,
        });
    }

    if let Some(rest) = header.strip_prefix("Message id=") {
        let (id, rest) = take_until(rest, " hop=")?;
        let (hop_str, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        let (hop, hop_max) = parse_hop(hop_str).unwrap_or((1, 10));
        return Some(PeerInboundKind::Message { id: id.to_owned(), from, org, hop, hop_max, body });
    }

    if let Some(rest) = header.strip_prefix("Reply id=") {
        let (id, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        return Some(PeerInboundKind::Reply { id: id.to_owned(), from, org, body });
    }

    if let Some(rest) = header.strip_prefix("Late reply id=") {
        let (id, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        return Some(PeerInboundKind::LateReply { id: id.to_owned(), from, org, body });
    }

    if let Some(rest) = header.strip_prefix("Ask id=") {
        // Caller-side timeout — `to agent 'X' (org 'Y') timed out ...`
        if let Some(rest_to) = rest_after_id(rest, " to agent ")
            && header.contains("timed out after 30 minutes")
        {
            let id = id_before(rest, " to agent ")?;
            let (target, org) = extract_from_agent_after(rest_to)?;
            return Some(PeerInboundKind::CallerTimeout { id: id.to_owned(), target, org, body });
        }
        // Caller-side delivery failure — `to agent 'X' (org 'Y') failed to deliver: <reason>`
        if let Some(rest_to) = rest_after_id(rest, " to agent ")
            && header.contains("failed to deliver:")
        {
            let id = id_before(rest, " to agent ")?;
            let (target, org, trailing) = extract_from_agent_after_with_trailer(rest_to)?;
            let reason = trailing
                .split_once("failed to deliver:")
                .map(|(_, after)| after.trim().to_owned())
                .unwrap_or_default();
            return Some(PeerInboundKind::DeliveryFailure {
                id: id.to_owned(),
                target,
                org,
                reason,
            });
        }
        // Recipient-side expired — `from agent 'A' (org 'O') has expired ...`
        if let Some(rest_from) = rest_after_id(rest, " from agent ")
            && header.contains("has expired")
        {
            let id = id_before(rest, " from agent ")?;
            let (from, org) = extract_from_agent_after(rest_from)?;
            return Some(PeerInboundKind::RecipientExpired { id: id.to_owned(), from, org, body });
        }
    }

    None
}

/// Detect an outbound peer tool_use card. Returns `None` for any
/// tool that isn't a peer ask/tell.
pub(crate) fn detect_outbound(tc: &ToolCallInfo) -> Option<PeerOutboundKind> {
    let raw = tc.raw_input.as_ref()?;
    let target = raw.get("target")?.as_str()?.to_owned();
    let correlation_id = extract_correlation_id_from_result(tc);
    match tc.sdk_tool_name.as_str() {
        "mcp__forge__peers__ask_agent" => {
            let body = raw.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Ask { target, body, correlation_id })
        }
        "mcp__forge__peers__tell_agent" => {
            let body = raw.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Tell { target, body, correlation_id })
        }
        _ => None,
    }
}

/// Build the styled lines for an inbound peer block. The chat
/// renderer calls this after `detect_inbound` matches and SKIPS the
/// default user-message rendering for the same text.
///
/// Visual shape mirrors a standard tool-call card: header
/// `  <status> <kind-icon-bold> <kind-label-bold> <sender bold orange> <meta dim…>`
/// then body lines under `  │  ` / `  └─ ` tree connectors —
/// peer rows visually rhyme with the tool rows above and below them.
pub(crate) fn render_inbound(kind: &PeerInboundKind, collapsed: bool) -> Vec<Line<'static>> {
    match kind {
        PeerInboundKind::Question { id, from, org, hop, hop_max, body } => render_peer_card(
            PeerCardStatus::Ok,
            INBOUND_ICON,
            theme::RUST_ORANGE,
            "question",
            from,
            &dim_meta(&[format!("· {id}"), format!("· hop {hop}/{hop_max}"), format!("· ({org})")]),
            body,
            collapsed,
        ),
        PeerInboundKind::Message { id, from, org, body, .. } => render_peer_card(
            PeerCardStatus::Ok,
            INBOUND_ICON,
            theme::SUBAGENT_TOKEN,
            "message",
            from,
            &dim_meta(&[format!("· {id}"), format!("· ({org})")]),
            body,
            collapsed,
        ),
        PeerInboundKind::Reply { id, from, org, body } => render_peer_card(
            PeerCardStatus::Ok,
            INBOUND_ICON,
            Color::Green,
            "reply",
            from,
            &dim_meta(&[format!("· {id}"), format!("· ({org})")]),
            body,
            collapsed,
        ),
        PeerInboundKind::LateReply { id, from, org, body } => {
            let mut meta = dim_meta(&[format!("· {id}")]);
            meta.push(Span::styled(" · ".to_owned(), Style::default().fg(theme::DIM)));
            meta.push(Span::styled(
                "\u{231b} late".to_owned(),
                Style::default().fg(theme::STATUS_WARNING),
            ));
            meta.push(Span::styled(format!(" · ({org})"), Style::default().fg(theme::DIM)));
            render_peer_card(
                PeerCardStatus::Warning,
                INBOUND_ICON,
                Color::Green,
                "reply",
                from,
                &meta,
                body,
                collapsed,
            )
        }
        PeerInboundKind::CallerTimeout { id, target, org, body } => render_peer_card(
            PeerCardStatus::Warning,
            INBOUND_ICON,
            theme::STATUS_ERROR,
            "ask timed out",
            target,
            &dim_meta(&[
                format!("· {id}"),
                "· no reply after 30 min".to_owned(),
                format!("· ({org})"),
            ]),
            body,
            collapsed,
        ),
        PeerInboundKind::RecipientExpired { id, from, org, body } => render_peer_card(
            PeerCardStatus::Warning,
            INBOUND_ICON,
            theme::STATUS_WARNING,
            "ask expired",
            from,
            &dim_meta(&[
                format!("· {id}"),
                "· your reply will be tagged late".to_owned(),
                format!("· ({org})"),
            ]),
            body,
            collapsed,
        ),
        PeerInboundKind::DeliveryFailure { id, target, org, reason } => render_peer_card(
            PeerCardStatus::Error,
            INBOUND_ICON,
            theme::STATUS_ERROR,
            "failed to deliver",
            target,
            &dim_meta(&[format!("· {id}"), format!("· ({org})")]),
            reason,
            collapsed,
        ),
    }
}

/// Build the styled lines for an outbound peer block. Used by the
/// chat renderer for `mcp__forge__peers__*` tool_use cards instead
/// of the default tool-card rendering. Same tool-card shape as
/// [`render_inbound`].
pub(crate) fn render_outbound(kind: &PeerOutboundKind, collapsed: bool) -> Vec<Line<'static>> {
    let (accent, label, target, body, id) = match kind {
        PeerOutboundKind::Ask { target, body, correlation_id } => {
            (theme::RUST_ORANGE, "ask", target, body, correlation_id)
        }
        PeerOutboundKind::Tell { target, body, correlation_id } => {
            (theme::SUBAGENT_TOKEN, "tell", target, body, correlation_id)
        }
    };
    let id_label = id.clone().unwrap_or_else(|| "q-…".to_owned());
    render_peer_card(
        PeerCardStatus::Ok,
        OUTBOUND_ICON,
        accent,
        label,
        target,
        &dim_meta(&[format!("· {id_label}")]),
        body,
        collapsed,
    )
}

/// Leading kind-icon glyphs for the header row. Outbound + inbound
/// are paired arrows pointing in opposite directions — same glyph
/// family so the two row shapes read as a consistent pair rather
/// than mixing arrow + triangle styles.
const INBOUND_ICON: &str = "\u{2190}"; // ←
const OUTBOUND_ICON: &str = "\u{2192}"; // →

/// Status-icon classification — drives the leading glyph + colour.
/// Mirrors the standard tool card's `✓` / `⚠` / `✗` semantics so a
/// glance picks up "happy / heads-up / broken" without reading the
/// label.
#[derive(Copy, Clone)]
enum PeerCardStatus {
    Ok,
    Warning,
    Error,
}

impl PeerCardStatus {
    const fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "\u{2713}",      // ✓
            Self::Warning => "\u{26a0}", // ⚠
            Self::Error => "\u{2717}",   // ✗
        }
    }

    const fn color(self) -> Color {
        match self {
            Self::Ok => Color::Green,
            Self::Warning => theme::STATUS_WARNING,
            Self::Error => theme::STATUS_ERROR,
        }
    }
}

/// Tool-card-shaped renderer shared by inbound + outbound peer
/// blocks. Header layout matches `tool_call::standard::render_tool_call_title`:
///
/// `  <status> <kind-icon BOLD coloured> <kind-label BOLD coloured> <name BOLD orange> <meta dim…>`
///
/// Body lines are pushed under `  │  ` / `  └─ ` tree connectors —
/// same glyph pair the standard tool card uses, so peer rows visually
/// rhyme with adjacent tool rows. When the body is empty (notice
/// variants without a trailing prose body), the header line stands
/// on its own with no tree underneath.
fn render_peer_card(
    status: PeerCardStatus,
    kind_icon: &str,
    kind_color: Color,
    label: &str,
    name: &str,
    meta_spans: &[Span<'static>],
    body: &str,
    collapsed: bool,
) -> Vec<Line<'static>> {
    let mut header = Line::default();
    header.spans.push(Span::raw("  "));
    header.spans.push(Span::styled(status.glyph().to_owned(), Style::default().fg(status.color())));
    header.spans.push(Span::raw(" "));
    header.spans.push(Span::styled(
        kind_icon.to_owned(),
        Style::default().fg(kind_color).add_modifier(Modifier::BOLD),
    ));
    header.spans.push(Span::raw(" "));
    header.spans.push(Span::styled(
        label.to_owned(),
        Style::default().fg(kind_color).add_modifier(Modifier::BOLD),
    ));
    header.spans.push(Span::raw(" "));
    header.spans.push(Span::styled(
        name.to_owned(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    ));
    for span in meta_spans {
        header.spans.push(span.clone());
    }

    let mut lines = vec![header];
    if collapsed {
        push_collapsed_summary(&mut lines, body);
    } else {
        push_tree_body_lines(&mut lines, body);
    }
    lines
}

/// One-line collapsed summary that matches the standard tool-card
/// shape: `  └─ <first line of body, truncated>  click or ctrl+x to expand`.
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
    line.spans.push(Span::styled("  \u{2514}\u{2500} ".to_owned(), dim));
    line.spans.push(Span::styled(summary, dim));
    line.spans.push(Span::styled("  click or ctrl+x to expand".to_owned(), dim));
    lines.push(line);
}

/// Build a flat list of DIM spans for a series of metadata fragments
/// shown in the header (e.g. `· q-7f3a92e0`, `· hop 1/10`, `· (Org)`).
/// Each fragment is rendered with the dim theme colour; the caller is
/// expected to embed the `·` separator prefix in the fragment string
/// itself. A single leading space precedes each fragment so they
/// visually separate from the bold name span.
fn dim_meta(fragments: &[String]) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(fragments.len() * 2);
    for fragment in fragments {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(fragment.clone(), Style::default().fg(theme::DIM)));
    }
    spans
}

/// Push the body lines under `│  ` / `└─ ` tree connectors —
/// matches `tool_call::standard::render_tool_content`'s pipe / corner
/// glyph pair. Renders the FULL body when expanded — no truncation;
/// the collapsed summary (see [`push_collapsed_summary`]) is the only
/// place we truncate, and only to fit a single summary row.
/// When the body is empty, pushes nothing so the header stands alone.
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
            "  \u{2514}\u{2500} " // └─
        } else {
            "  \u{2502}  " // │
        };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_owned(), pipe_style),
            Span::styled((*raw_line).to_owned(), body_text_style),
        ]));
    }
}

// ---------- parsing helpers ----------

/// Split `s` at the first occurrence of `marker`. Returns
/// `(before, after_marker)` — the marker itself is consumed.
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

/// After the `id=` slice has been peeled off, return the id portion
/// (everything before `marker`).
fn id_before<'a>(after_id: &'a str, marker: &str) -> Option<&'a str> {
    let idx = after_id.find(marker)?;
    Some(&after_id[..idx])
}

/// Given a slice starting at `'X' (org 'Y') ...`, extract `X` and
/// `Y`. Returns `None` when the format doesn't match.
fn extract_from_agent_after(rest: &str) -> Option<(String, String)> {
    let after_open = rest.strip_prefix('\'')?;
    let (name, after_name) = take_until(after_open, "' (org '")?;
    let (org, _trailing) = take_until(after_name, "')")?;
    Some((name.to_owned(), org.to_owned()))
}

/// Same as `extract_from_agent_after` but also returns the trailer —
/// the substring after `')`. Used by the delivery-failure parser
/// which needs name, org, AND the trailing reason text.
fn extract_from_agent_after_with_trailer(rest: &str) -> Option<(String, String, String)> {
    let after_open = rest.strip_prefix('\'')?;
    let (name, after_name) = take_until(after_open, "' (org '")?;
    let (org, trailing) = take_until(after_name, "')")?;
    Some((name.to_owned(), org.to_owned(), trailing.to_owned()))
}

/// Parse `k/M` into `(k, M)`. Tolerates bad input.
fn parse_hop(s: &str) -> Option<(u8, u8)> {
    let (a, b) = take_until(s, "/")?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Pull a correlation id out of a tool result's text (the JSON the
/// peer tools emit). Returns `None` when the call hasn't completed
/// yet, or when the parse fails.
fn extract_correlation_id_from_result(tc: &ToolCallInfo) -> Option<String> {
    use crate::agent::model::{ContentBlock, ToolCallContent};
    for content in &tc.content {
        if let ToolCallContent::Content(chunk) = content
            && let ContentBlock::Text(text_content) = &chunk.content
        {
            let text = &text_content.text;
            // Body is pretty-printed JSON containing
            //   "correlation_id": "q-XXXXXXXX"
            // Find that field. Don't full-parse — tool result strings
            // can be noisy at the edges.
            if let Some(start) = text.find("\"correlation_id\": \"") {
                let rest = &text[start + "\"correlation_id\": \"".len()..];
                if let Some(end) = rest.find('"') {
                    return Some(rest[..end].to_owned());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_question_inbound() {
        let text = "[Question id=q-7f3a92e0 hop=1/10 from agent 'forge' (org 'Personal') - reply with tell_agent in_reply_to=q-7f3a92e0]\n\nWhat's the test setup look like?";
        let kind = detect_inbound(text).expect("question parsed");
        match kind {
            PeerInboundKind::Question { id, from, org, hop, hop_max, body } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(from, "forge");
                assert_eq!(org, "Personal");
                assert_eq!(hop, 1);
                assert_eq!(hop_max, 10);
                assert_eq!(body, "What's the test setup look like?");
            }
            other => panic!("expected Question, got {other:?}"),
        }
    }

    #[test]
    fn detect_message_inbound() {
        let text = "[Message id=t-c45a8f12 hop=1/10 from agent 'forge' (org 'Personal')]\n\nFYI I just pushed the rewriter cleanup.";
        let kind = detect_inbound(text).expect("message parsed");
        match kind {
            PeerInboundKind::Message { id, from, org, body, .. } => {
                assert_eq!(id, "t-c45a8f12");
                assert_eq!(from, "forge");
                assert_eq!(org, "Personal");
                assert_eq!(body, "FYI I just pushed the rewriter cleanup.");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn detect_reply_inbound() {
        let text = "[Reply id=q-7f3a92e0 from agent 'granite-backend' (org 'Granite') to your earlier ask]\n\nWe use pgtemp.";
        let kind = detect_inbound(text).expect("reply parsed");
        match kind {
            PeerInboundKind::Reply { id, from, org, body } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(from, "granite-backend");
                assert_eq!(org, "Granite");
                assert_eq!(body, "We use pgtemp.");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn detect_late_reply_inbound() {
        let text = "[Late reply id=q-7f3a92e0 from agent 'granite-backend' (org 'Granite') - your ask expired before this reply was sent]\n\nSorry for the delay.";
        let kind = detect_inbound(text).expect("late reply parsed");
        assert!(matches!(kind, PeerInboundKind::LateReply { .. }));
    }

    #[test]
    fn detect_caller_timeout_inbound() {
        let text = "[Ask id=q-7f3a92e0 to agent 'granite-backend' (org 'Granite') timed out after 30 minutes - no reply received. Any reply after this point will be tagged late.]";
        let kind = detect_inbound(text).expect("caller timeout parsed");
        match kind {
            PeerInboundKind::CallerTimeout { id, target, org, .. } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(target, "granite-backend");
                assert_eq!(org, "Granite");
            }
            other => panic!("expected CallerTimeout, got {other:?}"),
        }
    }

    #[test]
    fn detect_recipient_expired_inbound() {
        let text = "[Ask id=q-7f3a92e0 from agent 'forge' (org 'Personal') has expired - any reply you produce will be tagged late.]";
        let kind = detect_inbound(text).expect("recipient expired parsed");
        match kind {
            PeerInboundKind::RecipientExpired { id, from, org, .. } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(from, "forge");
                assert_eq!(org, "Personal");
            }
            other => panic!("expected RecipientExpired, got {other:?}"),
        }
    }

    #[test]
    fn detect_delivery_failure_inbound() {
        let text = "[Ask id=q-d31fa8a3 to agent 'granite-liq-bot' (org 'Granite') failed to deliver: target spawn failed: all pinned accounts are rate-limited]";
        let kind = detect_inbound(text).expect("delivery failure parsed");
        match kind {
            PeerInboundKind::DeliveryFailure { id, target, reason, .. } => {
                assert_eq!(id, "q-d31fa8a3");
                assert_eq!(target, "granite-liq-bot");
                assert!(reason.contains("rate-limited"), "got: {reason}");
            }
            other => panic!("expected DeliveryFailure, got {other:?}"),
        }
    }

    /// I3 — cross-crate roundtrip guard. workspace's
    /// `WrappedPrompt::to_prose` (emitter) and forge-tui's
    /// `detect_inbound` (parser) hand-roll the same wire format in two
    /// crates. Without a roundtrip test, schema drift goes silent: a
    /// new field on one side keeps the other side's tests green while
    /// the live chat block stops rendering. These tests build a typed
    /// `WrappedPrompt` in workspace, emit it via `to_prose`, then feed
    /// the result to `detect_inbound` and assert the parsed kind
    /// matches what was emitted.
    #[test]
    fn roundtrip_question_to_prose_through_detect_inbound() {
        use forge_workspace::mcp::peers::types::{CorrelationId, WrappedKind, WrappedPrompt};
        let w = WrappedPrompt {
            correlation_id: CorrelationId::from_external("q-7f3a92e0").expect("valid id"),
            kind: WrappedKind::Question,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            hop: 1,
            hop_limit: 10,
            body: "What's the test setup look like?".to_owned(),
        };
        let kind = detect_inbound(&w.to_prose()).expect("roundtrip parses");
        match kind {
            PeerInboundKind::Question { id, from, org, hop, hop_max, body } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(from, "forge");
                assert_eq!(org, "Personal");
                assert_eq!(hop, 1);
                assert_eq!(hop_max, 10);
                assert_eq!(body, "What's the test setup look like?");
            }
            other => panic!("expected Question, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_message_to_prose_through_detect_inbound() {
        use forge_workspace::mcp::peers::types::{CorrelationId, WrappedKind, WrappedPrompt};
        let w = WrappedPrompt {
            correlation_id: CorrelationId::from_external("t-c45a8f12").expect("valid id"),
            kind: WrappedKind::Message,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            hop: 1,
            hop_limit: 10,
            body: "FYI I just pushed the rewriter cleanup.".to_owned(),
        };
        let kind = detect_inbound(&w.to_prose()).expect("roundtrip parses");
        assert!(matches!(kind, PeerInboundKind::Message { .. }));
    }

    #[test]
    fn roundtrip_reply_to_prose_through_detect_inbound() {
        use forge_workspace::mcp::peers::types::{CorrelationId, WrappedKind, WrappedPrompt};
        let w = WrappedPrompt {
            correlation_id: CorrelationId::from_external("q-7f3a92e0").expect("valid id"),
            kind: WrappedKind::Reply,
            sender_name: "granite-backend".to_owned(),
            sender_org: "Granite".to_owned(),
            hop: 0,
            hop_limit: 10,
            body: "We use pgtemp.".to_owned(),
        };
        let kind = detect_inbound(&w.to_prose()).expect("roundtrip parses");
        match kind {
            PeerInboundKind::Reply { id, from, org, body } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(from, "granite-backend");
                assert_eq!(org, "Granite");
                assert_eq!(body, "We use pgtemp.");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_delivery_failure_through_detect_inbound() {
        use forge_workspace::mcp::peers::types::{CorrelationId, WrappedKind, WrappedPrompt};
        let w = WrappedPrompt {
            correlation_id: CorrelationId::from_external("q-d31fa8a3").expect("valid id"),
            kind: WrappedKind::DeliveryFailureNotice,
            sender_name: "granite-liq-bot".to_owned(),
            sender_org: "Granite".to_owned(),
            hop: 0,
            hop_limit: 10,
            body: "target session connection lost".to_owned(),
        };
        let kind = detect_inbound(&w.to_prose()).expect("roundtrip parses");
        match kind {
            PeerInboundKind::DeliveryFailure { id, target, reason, .. } => {
                assert_eq!(id, "q-d31fa8a3");
                assert_eq!(target, "granite-liq-bot");
                assert!(reason.contains("connection lost"), "got: {reason}");
            }
            other => panic!("expected DeliveryFailure, got {other:?}"),
        }
    }

    #[test]
    fn detect_inbound_rejects_non_peer_text() {
        assert!(detect_inbound("Hello, world!").is_none());
        assert!(detect_inbound("[Not a peer wrapper]").is_none());
        assert!(detect_inbound("[Question with no id]\n\nbody").is_none());
    }

    #[test]
    fn render_inbound_question_produces_styled_lines() {
        let kind = PeerInboundKind::Question {
            id: "q-7f3a92e0".to_owned(),
            from: "forge".to_owned(),
            org: "Personal".to_owned(),
            hop: 1,
            hop_max: 10,
            body: "What's the setup?".to_owned(),
        };
        let lines = render_inbound(&kind, false);
        assert!(!lines.is_empty(), "non-empty");
        // First line is the header with the question icon + label.
        let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("question"), "header has label: {header_text}");
        assert!(header_text.contains("forge"), "header has sender: {header_text}");
        assert!(header_text.contains("q-7f3a92e0"), "header has id: {header_text}");
    }

    #[test]
    fn render_outbound_ask_block() {
        let kind = PeerOutboundKind::Ask {
            target: "granite-backend".to_owned(),
            body: "What's the test setup?".to_owned(),
            correlation_id: Some("q-7f3a92e0".to_owned()),
        };
        let lines = render_outbound(&kind, false);
        assert!(!lines.is_empty());
        let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("ask"), "header has label: {header_text}");
        assert!(header_text.contains("granite-backend"), "header has target: {header_text}");
        assert!(header_text.contains("q-7f3a92e0"), "header has id: {header_text}");
    }

    #[test]
    fn render_outbound_with_pending_correlation_id() {
        let kind = PeerOutboundKind::Tell {
            target: "forge".to_owned(),
            body: "fyi".to_owned(),
            correlation_id: None,
        };
        let lines = render_outbound(&kind, false);
        let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("tell"));
        // Pending id placeholder.
        assert!(header_text.contains("q-…"));
    }

    /// Expanded view must render the FULL body — no truncation,
    /// no ellipsis. Truncation only lives in the collapsed summary.
    #[test]
    fn expanded_body_is_not_truncated() {
        let body = "x".repeat(500);
        let kind = PeerInboundKind::Reply {
            id: "q-1".to_owned(),
            from: "a".to_owned(),
            org: "o".to_owned(),
            body: body.clone(),
        };
        let lines = render_inbound(&kind, false);
        let body_text: String =
            lines.iter().skip(1).flat_map(|l| l.spans.iter().map(|s| s.content.as_ref())).collect();
        assert!(
            !body_text.contains('\u{2026}'),
            "expanded body must not contain a truncation ellipsis: {body_text:?}"
        );
        // The full 500-char run should appear somewhere in the body
        // rows (it's a single line so it lands on one row).
        assert!(body_text.contains(body.as_str()), "expanded body must contain the full prose");
    }

    /// Collapsed inbound block should be exactly 2 lines: header +
    /// `  └─ <summary> click or ctrl+x to expand`.
    #[test]
    fn render_inbound_collapsed_shape() {
        let kind = PeerInboundKind::Question {
            id: "q-1".to_owned(),
            from: "forge".to_owned(),
            org: "Personal".to_owned(),
            hop: 1,
            hop_max: 10,
            body: "Line 1\nLine 2\nLine 3".to_owned(),
        };
        let lines = render_inbound(&kind, true);
        assert_eq!(lines.len(), 2, "collapsed = header + summary row");
        let last_text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(last_text.starts_with("  \u{2514}\u{2500} "), "tree corner present: {last_text}");
        assert!(last_text.contains("click or ctrl+x to expand"), "expand hint: {last_text}");
        assert!(last_text.contains("Line 1"), "summary shows first body line: {last_text}");
        assert!(!last_text.contains("Line 2"), "summary stops at first body line: {last_text}");
    }

    #[test]
    fn render_outbound_collapsed_shape() {
        let kind = PeerOutboundKind::Ask {
            target: "granite-backend".to_owned(),
            body: "Long prompt\nspanning\nmultiple lines".to_owned(),
            correlation_id: Some("q-1".to_owned()),
        };
        let lines = render_outbound(&kind, true);
        assert_eq!(lines.len(), 2);
        let last_text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(last_text.contains("click or ctrl+x to expand"));
    }
}
