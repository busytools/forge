//! The composer's live state: whether a seat is compacting, and the sign-in it
//! is waiting on.
//!
//! The wire announces each of these once and retains nothing, so what a
//! composer draws is folded here from the stream. A view that attaches to an
//! already-running session reads none of it from the core, which is why it
//! lives beside the surface rather than in the view.

use std::collections::{HashMap, HashSet};

use forge_primitives::Message;
use forge_primitives::SessionSlot;

use crate::SessionUpdate;

/// What the core's own reads cannot answer: the compaction the CLI announced
/// on the status frame, and the sign-in a seat is waiting on.
#[derive(Default, Clone)]
pub struct Composer {
    compacting: HashSet<SessionSlot>,
    sign_ins: HashMap<SessionSlot, SignIn>,
}

/// The sign-in a seat is waiting on, as the wire names it. The method is what
/// makes the hint say which account rather than only that one is needed.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SignIn {
    pub method_name: String,
    pub method_description: String,
}

impl Composer {
    /// Fold one update in: the compaction the CLI announced, and the sign-in a
    /// seat is waiting on.
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
}
