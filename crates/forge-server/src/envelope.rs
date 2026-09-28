//! Envelope parsing for the bracket-wrapped prose `forge_workspace`
//! injects into user-turn text, plus the Slack mrkdwn converter.
//!
//! Eight header shapes: the five peer kinds the workspace produces
//! (`Question`, `Message`, `Reply`, `DeliveryFailure`,
//! `WorkerSpawnFailed`) and the `Gotify`, `Cron` and `Slack` external
//! sources. Nothing here renders or holds state.

/// One inbound peer block parsed from the user-turn text.
///
/// Wire envelopes carry several fields (correlation id, originating
/// org) that the previous chrome surfaced as DIM meta chunks. The
/// redesigned chat block hides those by default - the parser still
/// skips past them in the prefix, but the type only retains what the
/// renderer or chat-streak grouping reads.
#[derive(Debug)]
pub enum PeerInboundKind {
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
    /// `[Slack - workspace 'X', <channel>] id ... ts ...` then one
    /// `<author>: <text> [ts ...]` line per delivered message - a
    /// conversation's news, delivered as a user turn. Rendered with the
    /// ◇ glyph + a `Slack` source label so it reads as an external event,
    /// not agent traffic. Never groups with peer envelopes (see
    /// [`PeerInboundKind::peer_sender_identity`]).
    Slack {
        workspace: String,
        channel: String,
        /// `None` when there is no name to print. The producer resolves the
        /// author before the prose is written, so a bot's message carries the
        /// `user` id it always had and its own profile name beside it; a name
        /// that resolved to nothing leaves `unknown` in the clause, which is
        /// dropped here. Also `None` for a multi-message body, whose members
        /// each name their own author.
        author: Option<String>,
        body: String,
    },
}

impl PeerInboundKind {
    /// The `sender_org` field threaded through every variant - drives
    /// same-project envelope grouping in the TUI's chat iteration.
    /// Variants that carry an explicit org return it; the
    /// worker-spawn-failure notice has no org of its own (it's
    /// lead-local, with no sending project) so it returns an empty
    /// string and naturally groups with adjacent lead-local envelopes.
    pub fn org(&self) -> &str {
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
    pub fn peer_sender_identity(&self) -> Option<&str> {
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

/// The envelope's own correlation id out of a peer wrapper header
/// (`[Message id=t-1a2b3c from agent ...]`). Unique per delivered
/// envelope, so it keys an inbound-led messaging group independently
/// of where the message sits in the session - unlike a positional key,
/// it survives history pruning and index shifts.
pub fn inbound_envelope_id(text: &str) -> Option<&str> {
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
pub fn detect_inbound(text: &str) -> Option<PeerInboundKind> {
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
        // The `id … ts …` tail (and a bundle's member count) sits past the
        // closing bracket, so the body is what follows the newline.
        let (tail, rest) = after_bracket.split_once('\n')?;
        // `slack_bundle_to_prose` writes `<author>: <text>` on every member
        // line, so a body without the separator is not this shape at all.
        let (author, body) = rest.split_once(": ")?;
        // Every member of a bundle names its own author, so the header names
        // none of them: promoting the first would read as the block's author
        // and lose the rest. The count is what says so - a single message's
        // own text may wrap just as a bundle's members do.
        if bundle_member_count(tail).is_some() {
            // Every member names its own author, and an id or the producer's
            // `unknown` placeholder is not a name - so the clause is dropped
            // line by line, exactly as the single-member header drops it.
            let body = rest
                .lines()
                .map(|line| {
                    line.split_once(": ").map_or(line, |(author, text)| {
                        if author == "unknown" || is_slack_id(author) { text } else { line }
                    })
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Some(PeerInboundKind::Slack {
                workspace: workspace.to_owned(),
                channel: channel.to_owned(),
                author: None,
                body,
            });
        }
        // A raw Slack id is not a name to print: the prose writes the
        // resolved author when there is one, and falls back to `message.user`
        // - a `U…` id - or to `unknown` when there is nothing at all.
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

/// Strip Slack's mrkdwn down to plain text: emphasis markers drop, a labelled
/// link becomes `label: url`, and the `&`/`<`/`>` entities decode.
pub fn tidy_mrkdwn(text: &str) -> String {
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

/// True for a raw Slack id (`C0C0T5E6RM1`, `U9ABCD`) rather than a name: an
/// uppercase first character followed by uppercase letters and digits only.
/// Both `message.user` and a DM's conversation label reach the prose as one
/// until the producer resolves a handle. The shape is a heuristic, not a
/// guarantee: an all-caps channel name matches it and loses its `#`.
pub fn is_slack_id(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|first| first.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// The member count a delivered bundle's header carries, and `None` for a
/// single message - which is what tells the two apart, since one message's
/// text may wrap over as many lines as a bundle's members do.
fn bundle_member_count(tail: &str) -> Option<usize> {
    tail.rsplit_once('(')?.1.strip_suffix(" messages)")?.parse().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_question_inbound() {
        let text = "[Question id=q-7f3a92e0 from agent 'forge' (org 'Personal') - reply with agents__tell in_reply_to=q-7f3a92e0]\n\nWhat's the test setup?";
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
    fn cron_has_no_peer_sender_identity() {
        // Grouping keys off peer_sender_identity; Cron returns None so it
        // never merges into a peer messaging group (mirrors Gotify).
        let kind = PeerInboundKind::Cron { prompt: "x".into() };
        assert_eq!(kind.peer_sender_identity(), None);
    }

    /// The shipped prose exactly as `slack_bundle_to_prose` emits it: the
    /// bracketed part carries the workspace and conversation, and the line
    /// under it is `<author>: <text>`. `unknown` is the producer's last
    /// resort when a message carries no author at all, and it is never
    /// printed as a name.
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

    /// Every member of a bundle carries its own `<author>: `, so the header
    /// names none of them: promoting the first would read as the block's
    /// author and silently drop the rest.
    #[test]
    fn a_bundle_header_names_no_author() {
        let prose = "[Slack - workspace 'acme', general] id C1 ts 2.2 (2 messages)\n\
                     alice: one [ts 1.1]\n\
                     bob: two [ts 2.2]";
        match detect_inbound(prose).expect("slack") {
            PeerInboundKind::Slack { author, body, .. } => {
                assert_eq!(author, None, "a multi-member body names its own authors");
                assert!(body.contains("alice: one"), "the body keeps every member: {body}");
            }
            other => panic!("expected Slack, got {other:?}"),
        }
    }

    /// One message's own text wraps over as many lines as a bundle's
    /// members do, so the member count - never the line count - is what
    /// tells the two apart.
    #[test]
    fn a_wrapped_single_message_is_not_read_as_a_bundle() {
        let text = "[Slack - workspace 'acme', general] id C1 ts 1.1\n\
                    alice: line one\nline two [ts 1.1]";
        match detect_inbound(text).expect("slack") {
            PeerInboundKind::Slack { author, body, .. } => {
                assert_eq!(author.as_deref(), Some("alice"), "one member keeps its header author");
                assert_eq!(body, "line one\nline two [ts 1.1]");
            }
            other => panic!("expected Slack, got {other:?}"),
        }
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
}
