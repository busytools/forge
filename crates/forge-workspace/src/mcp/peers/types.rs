//! Wire-shape types for the peer-coordination MCP feature.
//!
//! These types are workspace-internal - only `forge-workspace`
//! (the tool impls and the spawn-routing handlers) references them. Kept
//! out of `forge-primitives` because they never cross a crate boundary
//! (primitives is for cross-crate wire types only).
//!
//! ## Identity model
//!
//! A session is addressed by its slot: the project name, the org that
//! project belongs to, and a label, with `lead` naming the project's own
//! agent and a worker's label naming it. One project therefore holds many
//! addressable seats. All messages between sessions go through forge's
//! in-process MCP server (named `forge`) with the `agents__*` tools.
//!
//! ## Wire wrapping
//!
//! Every peer message that hits a recipient's chat is wrapped with a
//! prose header carrying the send's id and the sender's identity. The
//! recipient's LLM reads this header as part of its prompt context;
//! `forge_server::envelope::detect_inbound` matches the bracket prefix
//! and the recipient's TUI renders the envelope as a styled peer block
//! (`forge-tui::ui::peer_block`).

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::SessionSlot;

/// Typed id for one send. Format `m-XXXXXXXX`, where `XXXXXXXX` is 8
/// lowercase hex characters drawn from a fresh `Uuid::new_v4`.
///
/// Minted once at the sender's tool impl and threaded through the wrapper
/// text both sides read. It names the SEND - traceability for the echo the
/// sender gets back - never a conversation, which is why nothing parses it
/// off an inbound envelope to match it against outstanding state.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MessageId(pub String);

impl MessageId {
    /// Mint an id for one send.
    pub fn mint() -> Self {
        Self(format!("m-{}", hex_8()))
    }

    /// Borrow as a `&str` for logging / formatting.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex_8() -> String {
    let uuid = Uuid::new_v4();
    let s = uuid.simple().to_string();
    s[..8].to_owned()
}

/// The name a seat is shown by in an envelope header: the project for a
/// project's own agent, `project/label` for a worker.
///
/// One home, because a seat must name itself the same way whether it is
/// sending or being addressed - two spellings would render one worker as
/// two senders.
pub fn seat_name(slot: &SessionSlot) -> String {
    if slot.is_lead() {
        slot.project().to_owned()
    } else {
        format!("{}/{}", slot.project(), slot.label())
    }
}

/// Liveness of a project's own agent, as `agents__list` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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

/// Wire kind of a peer message. One conversational kind, because every
/// send is the same thing; the two notices are failures rather than
/// conversation and stay separate so a view can style them as such.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrappedKind {
    /// `agents__send_message` from sender. No reply is expected - a reply
    /// is just another message - so the envelope asks for nothing back.
    Message,
    /// forge-synthesised notice landing in the SENDER's chat when a
    /// message parked for a sleeping project never landed, because the
    /// spawn it was waiting on failed.
    DeliveryFailureNotice,
    /// forge-synthesised notice landing in the LEAD's chat when a
    /// worker's spawn failed asynchronously (subprocess crashed
    /// inside the `--worktree` machinery before reaching `Connected`).
    /// `sender_name` carries the worker label; `body` carries the
    /// reason text from the classifier (verbatim claude error).
    WorkerSpawnFailedNotice,
}

/// The complete content of an outgoing or inbound peer message.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WrappedPrompt {
    pub id: MessageId,
    pub kind: WrappedKind,
    pub sender_name: String,
    pub sender_org: String,
    pub body: String,
}

impl WrappedPrompt {
    /// Build the exact prose string that gets injected into the
    /// recipient's chat as a `Command::Prompt` text. The format MUST
    /// match the prefix patterns `forge_server::envelope::detect_inbound`
    /// looks for.
    pub fn to_prose(&self) -> String {
        match self.kind {
            WrappedKind::Message => format!(
                "[Message id={} from agent '{}' (org '{}')]\n\n{}",
                self.id, self.sender_name, self.sender_org, self.body,
            ),
            WrappedKind::DeliveryFailureNotice => format!(
                "[Message to agent '{}' (org '{}') failed to deliver: {}]",
                self.sender_name, self.sender_org, self.body,
            ),
            WrappedKind::WorkerSpawnFailedNotice => format!(
                "[Worker '{}' spawn failed id={}: {}]",
                self.sender_name, self.id, self.body,
            ),
        }
    }
}

/// Live snapshot of a project's own agent, as `agents__list` and
/// `agents__whoami` report it. Built fresh on each tool call from
/// forge.toml + workspace per-session state. Not persisted.
#[derive(Clone, Debug, Serialize)]
pub struct PeerStatus {
    /// Project name as configured in forge.toml.
    pub name: String,
    /// Org name the project belongs to (forge.toml `[[orgs]]`).
    pub org: String,
    /// Filesystem path to the project root.
    pub path: PathBuf,
    /// Current liveness - `Running` / `Sleeping`.
    pub status: PeerLiveness,
    /// When the session was first spawned in this forge process,
    /// or `None` if currently sleeping.
    pub spawned_at: Option<SystemTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_has_the_m_prefix() {
        let id = MessageId::mint();
        assert!(id.as_str().starts_with("m-"), "expected m- prefix, got: {id}");
        assert_eq!(id.as_str().len(), 10);
    }

    #[test]
    fn message_id_two_sends_are_unique() {
        assert_ne!(MessageId::mint(), MessageId::mint());
    }

    #[test]
    fn message_id_display_matches_inner_string() {
        let id = MessageId("m-abcd1234".to_owned());
        assert_eq!(id.to_string(), "m-abcd1234");
    }

    #[test]
    fn message_id_hex_chars_are_lowercase() {
        for _ in 0..50 {
            let id = MessageId::mint();
            let hex_part = &id.as_str()[2..];
            assert!(hex_part.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        }
    }

    fn wrapper(kind: WrappedKind, sender: &str, org: &str, body: &str) -> WrappedPrompt {
        WrappedPrompt {
            id: MessageId("m-7f3a92e0".to_owned()),
            kind,
            sender_name: sender.to_owned(),
            sender_org: org.to_owned(),
            body: body.to_owned(),
        }
    }

    #[test]
    fn wrapped_prompt_message_prose_is_the_bare_header() {
        // Every send is one kind now, so the header carries the id, the
        // sender and nothing else - no kind word, no reply instruction.
        let w = wrapper(
            WrappedKind::Message,
            "forge/steward",
            "Busytools",
            "FYI I just pushed the rewriter cleanup.",
        );
        assert_eq!(
            w.to_prose(),
            "[Message id=m-7f3a92e0 from agent 'forge/steward' (org 'Busytools')]\n\n\
             FYI I just pushed the rewriter cleanup.",
        );
    }

    #[test]
    fn wrapped_prompt_delivery_failure_notice_prose() {
        // The failure names the message that did not land, not an ask:
        // nothing is outstanding any more, so the header has no id.
        let w = wrapper(
            WrappedKind::DeliveryFailureNotice,
            "gateway-liq-bot",
            "Gateway",
            "target session connection lost",
        );
        assert_eq!(
            w.to_prose(),
            "[Message to agent 'gateway-liq-bot' (org 'Gateway') failed to deliver: \
             target session connection lost]",
        );
    }

    /// #146: WorkerSpawnFailedNotice prose carries the label as
    /// sender_name, the reason as body, and a synthetic correlation
    /// id so detect_inbound's parser can key on `id=`.
    #[test]
    fn wrapped_prompt_worker_spawn_failed_notice_prose() {
        let w = wrapper(
            WrappedKind::WorkerSpawnFailedNotice,
            "reviewer",
            "",
            "Failed to resolve base branch \"HEAD\": git rev-parse failed",
        );
        let prose = w.to_prose();
        assert_eq!(
            prose,
            "[Worker 'reviewer' spawn failed id=m-7f3a92e0: Failed to resolve base branch \"HEAD\": git rev-parse failed]",
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
