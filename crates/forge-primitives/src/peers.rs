//! Wire-shape types for the peer-coordination MCP feature (#114 v1).
//!
//! Cross-crate types only — no logic, no I/O, no async. Produced by
//! `forge-workspace::mcp::peers::*` (the four Tool impls) and consumed
//! by `forge-workspace` orchestration + `forge-tui::ui::peer_block`
//! rendering.
//!
//! ## Identity model
//!
//! A "peer agent" is one project session (as loaded from forge.toml).
//! v1 supports one session per project — the project name is the
//! stable identity that peers address each other by. All messages
//! between peers go through forge's in-process MCP server (named
//! `forge`) with the four `peers__*` tools.
//!
//! ## Wire wrapping
//!
//! Every peer message that hits a recipient's chat is wrapped with a
//! prose header carrying: correlation id, hop count, sender identity,
//! and (for asks) reply instructions. The recipient's LLM reads this
//! header as part of its prompt context; the recipient's TUI strips
//! the bracket prefix at render time and substitutes a styled peer
//! block (see `forge-tui::ui::peer_block`).
//!
//! Canonical reference for the wrapper text formats:
//! `.superpowers/brainstorm/peer-mcp-v1-mockup.html` section 6
//! (gitignored, local-only).

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::session_key::SessionKey;

/// Typed correlation id for an ask or tell. Format:
/// `q-XXXXXXXX` for asks, `t-XXXXXXXX` for tells, where `XXXXXXXX`
/// is 8 lowercase hex characters drawn from a fresh `Uuid::new_v4`.
/// Generated once at the sender's tool impl; threaded through the
/// wrapper text the recipient sees, then echoed back via
/// `in_reply_to` on the recipient's `tell_agent` reply.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(pub String);

impl CorrelationId {
    /// Mint a new ask correlation id (prefix `q-`).
    pub fn new_ask() -> Self {
        Self(format!("q-{}", hex_8()))
    }

    /// Mint a new tell correlation id (prefix `t-`).
    pub fn new_tell() -> Self {
        Self(format!("t-{}", hex_8()))
    }

    /// Borrow as a `&str` for logging / formatting.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True iff this id was minted by `new_ask` (or has the `q-` prefix).
    pub fn is_ask(&self) -> bool {
        self.0.starts_with("q-")
    }

    /// True iff this id was minted by `new_tell` (or has the `t-` prefix).
    pub fn is_tell(&self) -> bool {
        self.0.starts_with("t-")
    }
}

impl std::fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 8 lowercase hex characters from a fresh v4 UUID. ~4B possibilities
/// per prefix; collision probability is negligible at the scale of
/// in-flight asks per forge process.
fn hex_8() -> String {
    let uuid = Uuid::new_v4();
    let s = uuid.simple().to_string();
    s[..8].to_owned()
}

/// Liveness of a peer agent (= project) from the perspective of any
/// other agent calling `peers__list_agents`. Computed fresh on each
/// tool call from workspace state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerLiveness {
    /// Spawned and connected; ready to receive ask/tell immediately.
    Running,
    /// Configured in forge.toml but not currently spawned. Ask/tell
    /// will auto-spawn it via `Command::SpawnProject`.
    Sleeping,
    /// A recent spawn or connection attempt failed; the project is
    /// known to forge but not currently reachable. The next ask/tell
    /// will retry the spawn.
    Failed,
}

/// Reason why a peer message couldn't be delivered or got expired.
/// Either returned synchronously via `DeliverError` from the tool
/// impl, or carried in `SessionUpdate::PeerAskFailed` for async
/// failures detected after the tool already returned ok.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PeerFailureReason {
    /// Sleeping target's auto-spawn attempt failed (account picker
    /// exhausted, settings invalid, etc.). The `reason` string is
    /// LLM-readable detail.
    TargetSpawnFailed { reason: String },
    /// Target's session task crashed or was closed while the ask was
    /// in flight (caught via `SessionUpdate::ConnectionFailed` or
    /// `SessionTask::drop`).
    TargetConnectionFailed,
    /// Target's `command_sender` channel returned an error on send
    /// (typically during teardown). Distinct from `TargetConnectionFailed`
    /// only when the session-task hasn't formally signalled close yet.
    ChannelClosed,
}

/// Lifecycle status of an in-flight ask tracked in the workspace's
/// `inflight_asks` map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InflightStatus {
    /// Ask is open; awaiting reply within the 30-min budget.
    Pending,
    /// Ask exceeded its 30-min budget without a reply. Late replies
    /// are still delivered to the caller, tagged as `LateReply`.
    TimedOut,
    /// Recipient delivered a reply via `tell_agent { in_reply_to }`
    /// while the ask was still `Pending`.
    Replied,
    /// Target session was confirmed dead before a reply could arrive
    /// (auto-expired via `expire_target_inflight`).
    TargetFailed,
}

/// Wire kind of a peer message. The full prose wrapper (with id /
/// hop / sender) is built by `WrappedPrompt::to_prose`. Each kind
/// produces a distinct bracket prefix the recipient TUI's
/// `peer_block::detect_inbound` pattern-matches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrappedKind {
    /// `ask_agent` from sender. Recipient's LLM is expected to reply
    /// via `tell_agent` with `in_reply_to` set to this id.
    Question,
    /// Unsolicited `tell_agent` from sender (no reply expected),
    /// OR a degraded reply where `in_reply_to` didn't resolve.
    Message,
    /// `tell_agent` that's a reply to an earlier ask which is still
    /// `Pending` in `inflight_asks`.
    Reply,
    /// `tell_agent` that's a reply to an earlier ask which has
    /// already moved to `TimedOut`. Caller sees the LateReply tag.
    LateReply,
    /// forge-synthesised notice landing in the CALLER's chat when
    /// their outbound ask timed out. Bracket text: `[Ask id=X to
    /// agent 'B' (org 'O') timed out after 30 minutes ...]`.
    CallerTimeoutNotice,
    /// forge-synthesised notice landing in the RECIPIENT's chat when
    /// an inbound ask they were processing has expired on the caller
    /// side. Bracket text: `[Ask id=X from agent 'A' (org 'O') has
    /// expired ...]`.
    RecipientExpiredNotice,
    /// forge-synthesised notice landing in the CALLER's chat when
    /// delivery to the target failed (spawn failed, target crashed,
    /// channel closed). Bracket text: `[Ask id=X to agent 'B' (org
    /// 'O') failed to deliver: ...]`. The `body` carries the
    /// human-readable failure reason.
    DeliveryFailureNotice,
}

/// One in-flight peer ask tracked at the workspace level. Lives in
/// `Workspace.inflight_asks` keyed by `correlation_id`. Updated when
/// the timer fires, when a reply lands, when the target's session
/// ends, etc.
///
/// Note: the tokio `JoinHandle` for the 30-min timer lives in a
/// SEPARATE `Workspace.inflight_timers` HashMap (keeps forge-primitives
/// tokio-free; aborts are handled at the workspace layer).
#[derive(Clone, Debug)]
pub struct InflightAsk {
    pub correlation_id: CorrelationId,
    pub caller: SessionKey,
    pub caller_project: String,
    pub caller_org: String,
    pub target_project: String,
    pub queued_at: SystemTime,
    pub timeout_at: SystemTime,
    pub hop: u8,
    pub hop_limit: u8,
    pub status: InflightStatus,
}

/// The complete content of an outgoing or inbound peer message. Built
/// at the sender (forge-workspace::mcp::peers tool impls), rendered
/// into prose via `to_prose`, fed to the recipient's session as a
/// `Command::Prompt` text, and pattern-matched at render time on the
/// recipient's TUI to produce a styled peer block.
///
/// For "normal" kinds (Question / Message / Reply / LateReply) the
/// `sender_name` + `sender_org` fields identify who's writing AND the
/// "from agent 'X'" in the bracket text refers to that same sender.
///
/// For "synthetic" notice kinds (CallerTimeoutNotice /
/// RecipientExpiredNotice / DeliveryFailureNotice) the same fields
/// carry whichever party the bracket header refers to:
/// - `CallerTimeoutNotice` is shown to the caller; the bracket says
///   "to agent 'X'" where X is the target — so `sender_name` = target.
/// - `RecipientExpiredNotice` is shown to the recipient; the bracket
///   says "from agent 'X'" where X is the caller — so `sender_name` = caller.
/// - `DeliveryFailureNotice` is shown to the caller; the bracket says
///   "to agent 'X'" where X is the target — so `sender_name` = target.
///
/// The `body` field carries the user-body for normal kinds, or
/// human-readable reason detail for synthetic notices (empty string
/// allowed when the bracket alone is sufficient).
#[derive(Clone, Debug)]
pub struct WrappedPrompt {
    pub correlation_id: CorrelationId,
    pub kind: WrappedKind,
    pub sender_name: String,
    pub sender_org: String,
    pub hop: u8,
    pub hop_limit: u8,
    pub in_reply_to: Option<CorrelationId>,
    pub body: String,
}

impl WrappedPrompt {
    /// Build the exact prose string that gets injected into the
    /// recipient's chat as a `Command::Prompt` text. The format MUST
    /// match the prefix patterns `forge-tui::ui::peer_block::detect_inbound`
    /// looks for. Canonical reference: the mockup file
    /// `.superpowers/brainstorm/peer-mcp-v1-mockup.html` section 6.
    pub fn to_prose(&self) -> String {
        match self.kind {
            WrappedKind::Question => format!(
                "[Question id={} hop={}/{} from agent '{}' (org '{}') - reply with tell_agent in_reply_to={}]\n\n{}",
                self.correlation_id,
                self.hop,
                self.hop_limit,
                self.sender_name,
                self.sender_org,
                self.correlation_id,
                self.body,
            ),
            WrappedKind::Message => format!(
                "[Message id={} hop={}/{} from agent '{}' (org '{}')]\n\n{}",
                self.correlation_id,
                self.hop,
                self.hop_limit,
                self.sender_name,
                self.sender_org,
                self.body,
            ),
            WrappedKind::Reply => format!(
                "[Reply id={} from agent '{}' (org '{}') to your earlier ask]\n\n{}",
                self.correlation_id, self.sender_name, self.sender_org, self.body,
            ),
            WrappedKind::LateReply => format!(
                "[Late reply id={} from agent '{}' (org '{}') - your ask expired before this reply was sent]\n\n{}",
                self.correlation_id, self.sender_name, self.sender_org, self.body,
            ),
            WrappedKind::CallerTimeoutNotice => {
                let trailing =
                    if self.body.is_empty() { String::new() } else { format!("\n\n{}", self.body) };
                format!(
                    "[Ask id={} to agent '{}' (org '{}') timed out after 30 minutes - no reply received. Any reply after this point will be tagged late.]{}",
                    self.correlation_id, self.sender_name, self.sender_org, trailing,
                )
            }
            WrappedKind::RecipientExpiredNotice => {
                let trailing =
                    if self.body.is_empty() { String::new() } else { format!("\n\n{}", self.body) };
                format!(
                    "[Ask id={} from agent '{}' (org '{}') has expired - any reply you produce will be tagged late.]{}",
                    self.correlation_id, self.sender_name, self.sender_org, trailing,
                )
            }
            WrappedKind::DeliveryFailureNotice => format!(
                "[Ask id={} to agent '{}' (org '{}') failed to deliver: {}]",
                self.correlation_id, self.sender_name, self.sender_org, self.body,
            ),
        }
    }
}

/// Live snapshot of a peer agent returned by `peers__list_agents` or
/// `peers__whoami`. Built fresh on each tool call from forge.toml +
/// workspace per-session state. Not persisted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerStatus {
    /// Project name as configured in forge.toml.
    pub name: String,
    /// Org name the project belongs to (forge.toml `[[orgs]]`).
    pub org: String,
    /// Filesystem path to the project root.
    pub path: PathBuf,
    /// Current liveness — `Running` / `Sleeping` / `Failed`.
    pub status: PeerLiveness,
    /// Current model if the session is running, else `None`.
    pub model: Option<String>,
    /// Count of asks this session has received from peers that
    /// haven't been replied to yet.
    pub in_flight_incoming: usize,
    /// Count of asks this session has sent to peers that haven't
    /// received a reply yet.
    pub in_flight_outgoing: usize,
    /// When the session was first spawned in this forge process,
    /// or `None` if currently sleeping.
    pub spawned_at: Option<SystemTime>,
}

/// Per-session counters of pending peer activity. Maintained at the
/// workspace level (`peer_stats` map) and surfaced via
/// `SessionUpdate::PeerInflightStatsChanged` to drive sidebar badges
/// in the Projects pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInflightStats {
    /// Asks this session has sent to peers, awaiting reply.
    pub outgoing: usize,
    /// Asks this session has received from peers, awaiting our reply.
    pub incoming: usize,
    /// Asks this session sent that timed out without a reply
    /// (visible briefly in the sidebar badge before fading).
    pub timed_out: usize,
    /// Asks this session sent that failed to deliver
    /// (visible briefly in the sidebar badge before fading).
    pub delivery_failed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_id_new_ask_has_q_prefix() {
        let id = CorrelationId::new_ask();
        assert!(id.as_str().starts_with("q-"), "expected q- prefix, got: {id}");
        assert_eq!(id.as_str().len(), 10, "expected q- + 8 hex chars, got: {id}");
        assert!(id.is_ask());
        assert!(!id.is_tell());
    }

    #[test]
    fn correlation_id_new_tell_has_t_prefix() {
        let id = CorrelationId::new_tell();
        assert!(id.as_str().starts_with("t-"), "expected t- prefix, got: {id}");
        assert_eq!(id.as_str().len(), 10, "expected t- + 8 hex chars, got: {id}");
        assert!(id.is_tell());
        assert!(!id.is_ask());
    }

    #[test]
    fn correlation_id_two_asks_are_unique() {
        let a = CorrelationId::new_ask();
        let b = CorrelationId::new_ask();
        assert_ne!(a, b);
    }

    #[test]
    fn correlation_id_display_matches_inner_string() {
        let id = CorrelationId("q-abcd1234".to_owned());
        assert_eq!(id.to_string(), "q-abcd1234");
    }

    #[test]
    fn correlation_id_hex_chars_are_lowercase() {
        // Repeat 50× to ensure no uppercase variant slips through.
        for _ in 0..50 {
            let id = CorrelationId::new_ask();
            let hex_part = &id.as_str()[2..];
            assert!(
                hex_part.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "expected all-lowercase hex, got: {hex_part}",
            );
        }
    }

    fn wrapper(kind: WrappedKind, sender: &str, org: &str, body: &str) -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: CorrelationId(match kind {
                WrappedKind::Question
                | WrappedKind::Reply
                | WrappedKind::LateReply
                | WrappedKind::CallerTimeoutNotice
                | WrappedKind::RecipientExpiredNotice
                | WrappedKind::DeliveryFailureNotice => "q-7f3a92e0".to_owned(),
                WrappedKind::Message => "t-c45a8f12".to_owned(),
            }),
            kind,
            sender_name: sender.to_owned(),
            sender_org: org.to_owned(),
            hop: 1,
            hop_limit: 10,
            in_reply_to: None,
            body: body.to_owned(),
        }
    }

    #[test]
    fn wrapped_prompt_question_prose_matches_mockup() {
        let w =
            wrapper(WrappedKind::Question, "forge", "Personal", "What's the test setup look like?");
        let prose = w.to_prose();
        assert!(
            prose.starts_with(
                "[Question id=q-7f3a92e0 hop=1/10 from agent 'forge' (org 'Personal') - reply with tell_agent in_reply_to=q-7f3a92e0]",
            ),
            "got: {prose}",
        );
        assert!(prose.ends_with("What's the test setup look like?"));
    }

    #[test]
    fn wrapped_prompt_message_prose_matches_mockup() {
        let w = wrapper(
            WrappedKind::Message,
            "forge",
            "Personal",
            "FYI I just pushed the rewriter cleanup.",
        );
        let prose = w.to_prose();
        assert!(
            prose.starts_with(
                "[Message id=t-c45a8f12 hop=1/10 from agent 'forge' (org 'Personal')]"
            ),
            "got: {prose}",
        );
        assert!(prose.ends_with("FYI I just pushed the rewriter cleanup."));
    }

    #[test]
    fn wrapped_prompt_reply_prose_matches_mockup() {
        let w = wrapper(
            WrappedKind::Reply,
            "granite-backend",
            "Granite",
            "We use pgtemp for postgres fixtures.",
        );
        let prose = w.to_prose();
        assert!(
            prose.starts_with(
                "[Reply id=q-7f3a92e0 from agent 'granite-backend' (org 'Granite') to your earlier ask]",
            ),
            "got: {prose}",
        );
        assert!(prose.ends_with("We use pgtemp for postgres fixtures."));
    }

    #[test]
    fn wrapped_prompt_late_reply_prose_matches_mockup() {
        let w = wrapper(WrappedKind::LateReply, "granite-backend", "Granite", "We use pgtemp.");
        let prose = w.to_prose();
        assert!(
            prose.starts_with(
                "[Late reply id=q-7f3a92e0 from agent 'granite-backend' (org 'Granite') - your ask expired before this reply was sent]",
            ),
            "got: {prose}",
        );
    }

    #[test]
    fn wrapped_prompt_caller_timeout_notice_prose() {
        let w = wrapper(WrappedKind::CallerTimeoutNotice, "granite-backend", "Granite", "");
        let prose = w.to_prose();
        assert!(
            prose.starts_with(
                "[Ask id=q-7f3a92e0 to agent 'granite-backend' (org 'Granite') timed out after 30 minutes - no reply received.",
            ),
            "got: {prose}",
        );
        assert!(prose.ends_with("tagged late.]"), "no trailing body when empty; got: {prose}");
    }

    #[test]
    fn wrapped_prompt_recipient_expired_notice_prose() {
        let w = wrapper(WrappedKind::RecipientExpiredNotice, "forge", "Personal", "");
        let prose = w.to_prose();
        assert!(
            prose.starts_with(
                "[Ask id=q-7f3a92e0 from agent 'forge' (org 'Personal') has expired - any reply you produce will be tagged late.]",
            ),
            "got: {prose}",
        );
    }

    #[test]
    fn wrapped_prompt_delivery_failure_notice_prose() {
        let w = wrapper(
            WrappedKind::DeliveryFailureNotice,
            "granite-liq-bot",
            "Granite",
            "all pinned accounts are rate-limited (5h: Subspace 100%, Granite 100%)",
        );
        let prose = w.to_prose();
        assert!(
            prose.starts_with(
                "[Ask id=q-7f3a92e0 to agent 'granite-liq-bot' (org 'Granite') failed to deliver: all pinned accounts are rate-limited",
            ),
            "got: {prose}",
        );
    }

    #[test]
    fn peer_inflight_stats_default_is_zero() {
        let s = PeerInflightStats::default();
        assert_eq!(s.outgoing, 0);
        assert_eq!(s.incoming, 0);
        assert_eq!(s.timed_out, 0);
        assert_eq!(s.delivery_failed, 0);
    }

    #[test]
    fn peer_failure_reason_serde_round_trip() {
        let r = PeerFailureReason::TargetSpawnFailed { reason: "ratelimited".to_owned() };
        let json = serde_json::to_string(&r).expect("serialize");
        let back: PeerFailureReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
    }
}
