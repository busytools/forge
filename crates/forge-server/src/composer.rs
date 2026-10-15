//! The composer's live state: whether a seat is compacting, the sign-in it
//! is waiting on, and the landed dictation nobody has sent yet.
//!
//! The wire announces each of these once and retains nothing, so what a
//! composer draws is folded here from the stream. A view that attaches to an
//! already-running session reads none of it from the core, which is why it
//! lives beside the surface rather than in the view - and for the landed
//! words it is also the point: a page that reloads between the landing and
//! the send reads them back from the record rather than losing them (#1360).

use std::collections::{HashMap, HashSet};

use forge_primitives::Message;
use forge_primitives::SessionSlot;

use crate::SessionUpdate;

/// What the core's own reads cannot answer: the compaction the CLI announced
/// on the status frame, the sign-in a seat is waiting on, and the landed take
/// nobody sent.
#[derive(Default, Clone)]
pub struct Composer {
    compacting: HashSet<SessionSlot>,
    sign_ins: HashMap<SessionSlot, SignIn>,
    /// The last landed take's words per seat, kept until a prompt carries
    /// them away: the wire lands them once, to the connection that dictated,
    /// and a page that reloads before the send reads them back from here.
    landed: HashMap<SessionSlot, Landed>,
}

/// The sign-in a seat is waiting on, as the wire names it. The method is what
/// makes the hint say which account rather than only that one is needed.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SignIn {
    pub method_name: String,
    pub method_description: String,
}

/// A landed take's words, as the record re-hands them: what would have gone
/// into the draft, whether the take was partial, and the take's generation -
/// the landing memory's discriminator, so a re-hand is read as the same words
/// rather than a new landing.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Landed {
    pub text: String,
    pub truncated: bool,
    pub generation: u64,
}

impl Composer {
    /// Fold one update in: the compaction the CLI announced, the sign-in a
    /// seat is waiting on, and the landed take's words.
    pub fn apply(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AuthRequired { key, method_name, method_description } => {
                self.sign_ins.insert(
                    key.clone(),
                    SignIn {
                        method_name: method_name.clone(),
                        method_description: method_description.clone(),
                    },
                );
            }
            // **A landing is the seat's until it is sent** (#1360). The wire
            // hands one to its own dictating connection and retains nothing,
            // so a reload in between used to lose the words. A later failure
            // (any other outcome) leaves the older words standing - the
            // reader never sent them.
            SessionUpdate::DictateEnded {
                key,
                outcome: crate::surface::DictateOutcome::Landed { text, truncated },
                generation,
                ..
            } => {
                self.landed.insert(
                    key.clone(),
                    Landed { text: text.clone(), truncated: *truncated, generation: *generation },
                );
            }
            // **The dock's answer is a send too** (#1360): a reader who dictated
            // into an ask's own box sends the words through `Respond*`, which
            // writes no user row - so a resolution whose words ARE the landed
            // ones takes them with it. Any other answer leaves them: the
            // reader has still not sent those words.
            SessionUpdate::PendingInteractionResolved {
                key, answered_with: Some(words), ..
            } if self.landed.get(key).is_some_and(|landed| landed.text == *words) => {
                self.landed.remove(key);
            }
            // **A person's send takes the words away, and nothing else** (#1360).
            // A user row is a prompt OR a tool-result envelope - the CLI
            // answers every tool call with one, and a compaction or an
            // interrupt writes more - so the frame's kind alone would drop
            // the words seconds after they landed, mid-turn. `source` says
            // who dispatched it, and only `You` is a person's send (#2198's
            // rule): a tool result carries none, and forge's own machinery
            // (auto-continue, a worker kick) carries `Forge`.
            SessionUpdate::ChatAppended {
                key,
                source: Some(forge_workspace::PromptSource::You),
                ..
            } => {
                self.landed.remove(key);
            }
            // The CLI announces a compaction on the status frame and
            // clears it with a null, which is the only place either is
            // said. Everything else on the conversation is the chat's.
            SessionUpdate::ChatAppended { key, msg, .. } => {
                if let Message::System { subtype, data, .. } = msg
                    && subtype == "status"
                {
                    let field = data.get("status");
                    if field.and_then(serde_json::Value::as_str) == Some("compacting") {
                        self.compacting.insert(key.clone());
                    } else if field.is_some_and(serde_json::Value::is_null) {
                        self.compacting.remove(key);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn compacting(&self, slot: &SessionSlot) -> bool {
        self.compacting.contains(slot)
    }

    pub fn sign_in(&self, slot: &SessionSlot) -> Option<&SignIn> {
        self.sign_ins.get(slot)
    }

    /// The words a landed take left that nobody has sent yet, if there are any.
    pub fn landed(&self, slot: &SessionSlot) -> Option<&Landed> {
        self.landed.get(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::DictateOutcome;

    fn seat() -> SessionSlot {
        SessionSlot::lead("TestOrg", "proj")
    }

    fn landed(text: &str, generation: u64) -> SessionUpdate {
        SessionUpdate::DictateEnded {
            key: seat(),
            outcome: DictateOutcome::Landed { text: text.to_owned(), truncated: false },
            generation,
            initiator: None,
        }
    }

    fn a_prompt() -> SessionUpdate {
        let msg: Message = serde_json::from_value(serde_json::json!({
            "type": "user",
            "session_id": "s1",
            "message": {"role": "user", "content": "sent"},
        }))
        .expect("a user frame");
        SessionUpdate::ChatAppended {
            key: seat(),
            msg,
            origin: Some(forge_workspace::PromptOrigin::View),
            source: Some(forge_workspace::PromptSource::You),
        }
    }

    fn a_user_row() -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "session_id": "s1",
            "message": {"role": "user", "content": "words"},
        }))
        .expect("a user frame")
    }

    /// A tool-result envelope: the CLI answers every tool call with one of
    /// these, and it is a user row.
    fn a_tool_result() -> SessionUpdate {
        let msg: Message = serde_json::from_value(serde_json::json!({
            "type": "user",
            "session_id": "s1",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1", "content": "ok"},
            ]},
        }))
        .expect("a tool-result frame");
        SessionUpdate::ChatAppended { key: seat(), msg, origin: None, source: None }
    }

    #[test]
    fn a_tool_result_does_not_take_the_words() {
        let mut composer = Composer::default();
        composer.apply(&landed("mine", 1));
        composer.apply(&a_tool_result());
        assert_eq!(
            composer.landed(&seat()).map(|words| words.text.as_str()),
            Some("mine"),
            "the CLI answers every tool call with a user row - a turn's own work must not \
             swallow words the reader has not sent",
        );
    }

    #[test]
    fn an_answer_carrying_the_landed_words_takes_them() {
        let mut composer = Composer::default();
        composer.apply(&landed("the dictated answer", 5));
        composer.apply(&SessionUpdate::PendingInteractionResolved {
            key: seat(),
            tool_id: "tu-ask".to_owned(),
            question_index: None,
            answered_with: Some("a different pick's words".to_owned()),
        });
        assert_eq!(
            composer.landed(&seat()).map(|words| words.text.as_str()),
            Some("the dictated answer"),
            "an answer that is not the landed words leaves them - the reader has not sent them",
        );
        composer.apply(&SessionUpdate::PendingInteractionResolved {
            key: seat(),
            tool_id: "tu-ask".to_owned(),
            question_index: None,
            answered_with: Some("the dictated answer".to_owned()),
        });
        assert!(
            composer.landed(&seat()).is_none(),
            "and the answer carrying exactly them takes them, which is #1309's rule kept",
        );
    }

    #[test]
    fn forges_own_machinery_does_not_take_the_words() {
        let mut composer = Composer::default();
        composer.apply(&landed("mine", 1));
        composer.apply(&SessionUpdate::ChatAppended {
            key: seat(),
            msg: a_user_row(),
            origin: None,
            source: Some(forge_workspace::PromptSource::Forge),
        });
        assert_eq!(
            composer.landed(&seat()).map(|words| words.text.as_str()),
            Some("mine"),
            "a worker kick or an auto-continue is not the reader's send",
        );
    }

    #[test]
    fn a_landing_is_kept_until_a_prompt_carries_it_away() {
        let mut composer = Composer::default();
        composer.apply(&landed("first words", 3));
        let kept = composer.landed(&seat()).expect("the words are kept");
        assert_eq!(kept.text, "first words");
        assert_eq!(kept.generation, 3, "with the take's own number");

        composer.apply(&a_prompt());
        assert!(
            composer.landed(&seat()).is_none(),
            "the sent prompt takes the words with it, so nothing already sent is handed back",
        );
    }

    #[test]
    fn a_failed_take_leaves_the_unsent_words_standing() {
        let mut composer = Composer::default();
        composer.apply(&landed("mine", 1));
        composer.apply(&SessionUpdate::DictateEnded {
            key: seat(),
            outcome: DictateOutcome::Failed,
            generation: 2,
            initiator: None,
        });
        assert_eq!(
            composer.landed(&seat()).map(|words| words.text.as_str()),
            Some("mine"),
            "the reader never sent the older words, so a later failure leaves them",
        );
    }

    #[test]
    fn a_newer_landing_replaces_the_older_words() {
        let mut composer = Composer::default();
        composer.apply(&landed("first", 1));
        composer.apply(&landed("second", 2));
        assert_eq!(
            composer.landed(&seat()).map(|words| words.text.as_str()),
            Some("second"),
            "the newest landing is what the record re-hands",
        );
    }
}
