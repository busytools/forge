//! Wire-shape types for the peer-coordination MCP feature.
//!
//! These types are workspace-internal - only `forge-workspace`
//! (the tool impls, the workspace's inflight tracking, the spawn-
//! routing handlers) references them. Kept out of `forge-primitives`
//! because they never cross a crate boundary (primitives is for
//! cross-crate wire types only).
//!
//! The one truly cross-crate peer type - `PeerInflightStats` -
//! stays in `forge-primitives` because the TUI reads it through
//! `SessionUpdate::PeerInflightStatsChanged`.
//!
//! ## Identity model
//!
//! A "peer agent" is one project session (as loaded from forge.toml).
//! v1 supports one session per project - the project name is the
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

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::SessionKey;

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

    /// Validate an LLM-supplied correlation id at the tool boundary.
    /// Format: `q-` or `t-` prefix + 8 lowercase hex characters.
    /// Returns None on any deviation - tools reject the call with
    /// is_error instead of letting a malformed id miss the inflight
    /// map silently (which would degrade a Reply to a Message and
    /// hide the actual problem).
    pub fn from_external(s: &str) -> Option<Self> {
        if s.len() != 10 {
            return None;
        }
        let prefix_ok = s.starts_with("q-") || s.starts_with("t-");
        if !prefix_ok {
            return None;
        }
        if !s[2..].chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) {
            return None;
        }
        Some(Self(s.to_owned()))
    }
}

impl std::fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex_8() -> String {
    let uuid = Uuid::new_v4();
    let s = uuid.simple().to_string();
    s[..8].to_owned()
}

/// Liveness of a peer agent (= project) from the perspective of any
/// other agent calling `peers__list_agents`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerLiveness {
    /// Spawned and connected; ready to receive ask/tell immediately.
    Running,
    /// Configured in forge.toml but not currently spawned. Ask/tell
    /// will auto-spawn it via `Command::SpawnProject`.
    Sleeping,
}

/// Reason why a peer message couldn't be delivered. Carried in the
/// `DeliveryFailureNotice` wrapper dispatched to the caller's chat
/// when target-crash detection fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PeerFailureReason {
    /// Target's session task crashed or was closed while the ask was
    /// in flight.
    TargetConnectionFailed,
}

/// Which MCP a cross-agent ask/message travelled over. Drives the
/// reply-tool named in the Question envelope and the same-channel /
/// wrong-channel reply-routing decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskChannel {
    Peers,
    Workers,
}

impl AskChannel {
    /// The tell-tool an inbound Question on this channel expects a reply through.
    pub fn reply_tool(self) -> &'static str {
        match self {
            AskChannel::Peers => "peers__tell_agent",
            AskChannel::Workers => "workers__tell",
        }
    }
}

/// Wire kind of a peer message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrappedKind {
    /// `ask_agent` from sender. Recipient replies via the channel's
    /// tell-tool with `in_reply_to` set to this id.
    Question,
    /// Unsolicited `tell_agent` from sender (no reply expected),
    /// OR a degraded reply where `in_reply_to` didn't resolve.
    Message,
    /// `tell_agent` that's a reply to an earlier ask.
    Reply,
    /// forge-synthesised notice landing in the CALLER's chat when
    /// delivery to the target failed (target crashed mid-flight,
    /// session connection closed).
    DeliveryFailureNotice,
    /// forge-synthesised notice landing in the LEAD's chat when a
    /// worker's spawn failed asynchronously (subprocess crashed
    /// inside the `--worktree` machinery before reaching `Connected`).
    /// `sender_name` carries the worker label; `body` carries the
    /// reason text from the classifier (verbatim claude error).
    WorkerSpawnFailedNotice,
}

/// One in-flight peer ask tracked at the workspace level. Lives in
/// `Workspace.inflight_asks` keyed by `correlation_id`; presence in
/// the map is the lifecycle signal - the entry is removed on reply
/// (`complete_inflight_ask`) or target-failure
/// (`expire_inflight_ask_failed`).
#[derive(Clone, Debug)]
pub struct InflightAsk {
    pub correlation_id: CorrelationId,
    /// Which MCP this ask travelled over. A reply's `in_reply_to`
    /// routes by this channel: a same-channel reply goes straight to
    /// `caller`; a reply arriving on the other channel is rejected
    /// with a steer to the right tool.
    pub channel: AskChannel,
    pub caller: SessionKey,
    pub target_project: String,
    /// Session stamped with this ask's `IncomingPlus1` at delivery
    /// (`None` until delivered) so expiry can clear the target's
    /// incoming badge, not just the caller's outgoing.
    pub target_session: Option<SessionKey>,
}

/// Outcome of classifying a `tell` with an optional `in_reply_to`
/// against the shared inflight-ask map. Drives the three-way routing
/// decision in both MCP tell handlers.
pub(crate) enum ReplyRouting {
    /// `in_reply_to` resolved to an ask on THIS channel: route the
    /// Reply straight to `caller`'s session, closing `correlation`.
    Reply { caller: SessionKey, correlation: CorrelationId },
    /// No `in_reply_to`, or one that resolved to no open ask: deliver
    /// as an unsolicited Message to the tell's declared target.
    Message,
    /// `in_reply_to` resolved to an ask on the OTHER channel: reject
    /// with a steer naming `correct_tool`.
    WrongChannel { correct_tool: &'static str },
}

/// The complete content of an outgoing or inbound peer message.
#[derive(Clone, Debug)]
pub struct WrappedPrompt {
    pub correlation_id: CorrelationId,
    pub kind: WrappedKind,
    /// Channel this message travelled over. The Question envelope
    /// names the matching reply-tool so the recipient replies through
    /// the same MCP the ask arrived on.
    pub channel: AskChannel,
    pub sender_name: String,
    pub sender_org: String,
    pub hop: u8,
    pub hop_limit: u8,
    pub body: String,
}

impl WrappedPrompt {
    /// True when this envelope has been relayed past its hop limit -
    /// the anti-relay-cycle guard every delivery path checks before
    /// forwarding.
    pub(crate) fn exceeds_hop_limit(&self) -> bool {
        self.hop > self.hop_limit
    }

    /// Build the exact prose string that gets injected into the
    /// recipient's chat as a `Command::Prompt` text. The format MUST
    /// match the prefix patterns `forge-tui::ui::peer_block::detect_inbound`
    /// looks for.
    pub fn to_prose(&self) -> String {
        match self.kind {
            WrappedKind::Question => format!(
                "[Question id={} hop={}/{} from agent '{}' (org '{}') - reply with {} in_reply_to={}]\n\n{}",
                self.correlation_id,
                self.hop,
                self.hop_limit,
                self.sender_name,
                self.sender_org,
                self.channel.reply_tool(),
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
            WrappedKind::DeliveryFailureNotice => format!(
                "[Ask id={} to agent '{}' (org '{}') failed to deliver: {}]",
                self.correlation_id, self.sender_name, self.sender_org, self.body,
            ),
            WrappedKind::WorkerSpawnFailedNotice => format!(
                "[Worker '{}' spawn failed id={}: {}]",
                self.sender_name, self.correlation_id, self.body,
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
    /// Current liveness - `Running` / `Sleeping`.
    pub status: PeerLiveness,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_id_new_ask_has_q_prefix() {
        let id = CorrelationId::new_ask();
        assert!(id.as_str().starts_with("q-"), "expected q- prefix, got: {id}");
        assert_eq!(id.as_str().len(), 10);
    }

    #[test]
    fn correlation_id_new_tell_has_t_prefix() {
        let id = CorrelationId::new_tell();
        assert!(id.as_str().starts_with("t-"));
        assert_eq!(id.as_str().len(), 10);
    }

    #[test]
    fn correlation_id_from_external_validates_shape() {
        assert_eq!(
            CorrelationId::from_external("q-abcd1234"),
            Some(CorrelationId("q-abcd1234".to_owned())),
        );
        assert_eq!(
            CorrelationId::from_external("t-deadbeef"),
            Some(CorrelationId("t-deadbeef".to_owned())),
        );
        assert_eq!(CorrelationId::from_external("x-abcd1234"), None);
        assert_eq!(CorrelationId::from_external("qabcd1234"), None);
        assert_eq!(CorrelationId::from_external("q-abc"), None);
        assert_eq!(CorrelationId::from_external("q-abcd12345"), None);
        assert_eq!(CorrelationId::from_external("q-ABCD1234"), None);
        assert_eq!(CorrelationId::from_external("q-zzzzzzzz"), None);
        assert_eq!(CorrelationId::from_external(""), None);
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
        for _ in 0..50 {
            let id = CorrelationId::new_ask();
            let hex_part = &id.as_str()[2..];
            assert!(hex_part.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        }
    }

    fn wrapper(
        kind: WrappedKind,
        channel: AskChannel,
        sender: &str,
        org: &str,
        body: &str,
    ) -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: CorrelationId(match kind {
                WrappedKind::Question
                | WrappedKind::Reply
                | WrappedKind::DeliveryFailureNotice
                | WrappedKind::WorkerSpawnFailedNotice => "q-7f3a92e0".to_owned(),
                WrappedKind::Message => "t-c45a8f12".to_owned(),
            }),
            kind,
            channel,
            sender_name: sender.to_owned(),
            sender_org: org.to_owned(),
            hop: 1,
            hop_limit: 10,
            body: body.to_owned(),
        }
    }

    #[test]
    fn wrapped_prompt_question_prose_names_peers_reply_tool() {
        let w = wrapper(
            WrappedKind::Question,
            AskChannel::Peers,
            "forge",
            "Personal",
            "What's the test setup look like?",
        );
        let prose = w.to_prose();
        assert!(prose.starts_with(
            "[Question id=q-7f3a92e0 hop=1/10 from agent 'forge' (org 'Personal') - reply with peers__tell_agent in_reply_to=q-7f3a92e0]",
        ));
        assert!(prose.ends_with("What's the test setup look like?"));
    }

    #[test]
    fn wrapped_prompt_question_prose_names_workers_reply_tool() {
        let w = wrapper(
            WrappedKind::Question,
            AskChannel::Workers,
            "lead",
            "Personal",
            "Status on the failing test?",
        );
        let prose = w.to_prose();
        assert!(prose.starts_with(
            "[Question id=q-7f3a92e0 hop=1/10 from agent 'lead' (org 'Personal') - reply with workers__tell in_reply_to=q-7f3a92e0]",
        ));
        assert!(prose.ends_with("Status on the failing test?"));
    }

    #[test]
    fn wrapped_prompt_message_prose_matches_mockup() {
        let w = wrapper(
            WrappedKind::Message,
            AskChannel::Peers,
            "forge",
            "Personal",
            "FYI I just pushed the rewriter cleanup.",
        );
        let prose = w.to_prose();
        assert!(
            prose.starts_with(
                "[Message id=t-c45a8f12 hop=1/10 from agent 'forge' (org 'Personal')]"
            )
        );
    }

    #[test]
    fn wrapped_prompt_reply_prose_matches_mockup() {
        let w = wrapper(
            WrappedKind::Reply,
            AskChannel::Peers,
            "gateway-backend",
            "Gateway",
            "We use pgtemp for postgres fixtures.",
        );
        let prose = w.to_prose();
        assert!(prose.starts_with(
            "[Reply id=q-7f3a92e0 from agent 'gateway-backend' (org 'Gateway') to your earlier ask]",
        ));
    }

    #[test]
    fn wrapped_prompt_delivery_failure_notice_prose() {
        let w = wrapper(
            WrappedKind::DeliveryFailureNotice,
            AskChannel::Peers,
            "gateway-liq-bot",
            "Gateway",
            "target session connection lost",
        );
        let prose = w.to_prose();
        assert!(prose.starts_with(
            "[Ask id=q-7f3a92e0 to agent 'gateway-liq-bot' (org 'Gateway') failed to deliver: target session connection lost",
        ));
    }

    /// #146: WorkerSpawnFailedNotice prose carries the label as
    /// sender_name, the reason as body, and a synthetic correlation
    /// id so detect_inbound's parser can key on `id=`.
    #[test]
    fn wrapped_prompt_worker_spawn_failed_notice_prose() {
        let w = wrapper(
            WrappedKind::WorkerSpawnFailedNotice,
            AskChannel::Workers,
            "reviewer",
            "",
            "Failed to resolve base branch \"HEAD\": git rev-parse failed",
        );
        let prose = w.to_prose();
        assert_eq!(
            prose,
            "[Worker 'reviewer' spawn failed id=q-7f3a92e0: Failed to resolve base branch \"HEAD\": git rev-parse failed]",
        );
    }

    #[test]
    fn peer_failure_reason_serde_round_trip() {
        let r = PeerFailureReason::TargetConnectionFailed;
        let json = serde_json::to_string(&r).expect("serialize");
        let back: PeerFailureReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
    }
}
