//! The handoff record: what one session swap carries, and the numbers it
//! leaves behind for tuning the trigger.

use serde::{Deserialize, Serialize};

use crate::messages::Message;
use crate::session_slot::SessionSlot;

/// Why a session handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffReason {
    /// The seat's context fill crossed its configured share of the window.
    Fill,
    /// A compaction fired before a turn boundary could carry the handoff.
    Compaction,
    /// A person asked for one.
    #[default]
    Manual,
}

/// One labelled block of the brief.
///
/// Items rather than a string so the row draws the same facts the new
/// session was handed, instead of parsing prose back apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffSection {
    pub label: String,
    pub lines: Vec<String>,
}

/// The two halves a successor is given: forge's own facts, then the
/// handing-over session's judgment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffBrief {
    pub ground_truth: Vec<HandoffSection>,
    pub judgment: Vec<HandoffSection>,
}

/// The cycle a handoff ended, read from the handover's own transcript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffCycle {
    pub requests: u32,
    pub turns: u32,
    pub prompt_tokens: u64,
}

/// How the successor's first ten turns went, which is what says whether the
/// handoff fired too early (a successor re-reading everything) or too late
/// (a compaction firing first).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffSuccessor {
    /// Turns counted before the measurement settled.
    pub turns: u32,
    /// Files the successor read that its predecessor had already read.
    pub reread_files: u32,
    /// Questions the successor asked.
    pub questions: u32,
    /// User messages that corrected or repeated an instruction.
    pub corrections: u32,
}

/// The working tree as the brief reads it, from three cheap git probes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffGitState {
    /// The current branch, `None` outside a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `HEAD` in short form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// `git status --porcelain` lines, capped.
    #[serde(default)]
    pub dirty: Vec<String>,
    /// How many dirty paths there were before the cap, so the brief can say
    /// what it is not showing.
    #[serde(default)]
    pub dirty_total: u32,
    /// `git diff --stat` lines.
    #[serde(default)]
    pub diff: Vec<String>,
}

/// One handoff, as the row draws it and as the store keeps it for tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffRecord {
    pub slot: SessionSlot,
    /// The session that handed over.
    pub from: String,
    /// The session that continues.
    pub to: String,
    pub reason: HandoffReason,
    /// The fill reading the trigger read, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// The configured share of the window when this fired, so a later config
    /// change does not rewrite what the record means.
    pub at_percent: u8,
    pub brief: HandoffBrief,
    /// The turn the handing-over session ran the procedure in: the prompt
    /// forge wrote, the calls it made and the exit call.
    ///
    /// **Carried because the transcript that holds it belongs to a session a
    /// view has left.** A client attaching after the swap reads the
    /// successor's history and would draw only the row; the turn's own frames
    /// come from here. Empty for a handover whose brief was supplied without
    /// a procedure turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judgment_turn: Vec<Message>,
    /// The handover's transcript, which holds everything the brief does not.
    pub transcript_path: String,
    /// RFC 3339.
    pub at: String,
    #[serde(default)]
    pub cycle: HandoffCycle,
    /// Absent until the successor has run its first ten turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<HandoffSuccessor>,
}

impl HandoffRecord {
    /// The row's one-line address for a view: the pair it swapped.
    pub fn pair(&self) -> String {
        format!("{} -> {}", short_id(&self.from), short_id(&self.to))
    }
}

/// The short form a row names a session by - the first eight characters,
/// which is how transcripts are told apart in this project already.
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> HandoffRecord {
        HandoffRecord {
            slot: SessionSlot::lead("Busytools", "forge"),
            from: "9cfe315c-1111-2222-3333-444444444444".to_owned(),
            to: "4b21e0a7-5555-6666-7777-888888888888".to_owned(),
            reason: HandoffReason::Fill,
            fill_percent: Some(85),
            total_tokens: Some(867_000),
            max_tokens: Some(1_020_000),
            at_percent: 85,
            brief: HandoffBrief::default(),
            judgment_turn: Vec::new(),
            transcript_path: "/tmp/9cfe315c.jsonl".to_owned(),
            at: "2026-10-24T08:00:00Z".to_owned(),
            cycle: HandoffCycle { requests: 514, turns: 96, prompt_tokens: 4_970_000_000 },
            successor: None,
        }
    }

    /// The row is addressed by the pair, and by the short form a reader
    /// recognises from a transcript name.
    #[test]
    fn the_pair_names_both_sessions_short() {
        assert_eq!(record().pair(), "9cfe315c -> 4b21e0a7");
    }

    /// The record crosses the wire and the store with its numbers intact, and
    /// an unmeasured successor stays absent rather than reading as zeros.
    #[test]
    fn the_record_round_trips_with_an_unmeasured_successor() {
        let json = serde_json::to_value(record()).expect("the record serializes");
        let successor = json.get("successor");
        assert!(successor.is_none(), "an unmeasured successor is not written");
        let back: HandoffRecord = serde_json::from_value(json).expect("and reads back");
        assert_eq!(back, record(), "the record survives the trip unchanged");
    }

    /// A record written before a field existed still reads, which is what
    /// keeps an old store row drawable.
    #[test]
    fn a_record_without_the_optional_numbers_reads() {
        let json = serde_json::json!({
            "slot": {"org": "Busytools", "project": "forge", "label": "lead"},
            "from": "a", "to": "b", "reason": "manual",
            "at_percent": 85,
            "brief": {"ground_truth": [], "judgment": []},
            "transcript_path": "/tmp/a.jsonl", "at": "now"
        });
        let back: HandoffRecord = serde_json::from_value(json).expect("the record reads");
        assert_eq!(back.reason, HandoffReason::Manual);
        assert_eq!(back.fill_percent, None, "no reading is not a reading of zero");
        assert_eq!(back.cycle.requests, 0, "and an absent cycle reads empty");
    }
}
