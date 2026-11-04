//! The vault family's ask shape: the draft one vault call parks for the
//! reader's approval, the answer a person gives it, and the tools those
//! calls name.
//!
//! **No secret value is a field here.** The draft carries what a card draws:
//! the tool, the item, the field being revealed or changed, a masked diff, and
//! an attachment's name and size. Nothing here is a value a person would learn
//! by reading it. The value itself never leaves the process except through the
//! tool's own result after an approval.
//!
//! The shared session's lock state is not frozen into the draft: the core
//! fills [`VaultDraft::needs_unlock`] when it builds the card from the live
//! session, so a second parked call re-bases to an ordinary approve card the
//! moment another call unlocks the session.

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

/// One vault tool a held call can name. The wire spelling is the MCP tool
/// name, so the card and the agent's request cannot disagree about which
/// operation is being approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultTool {
    #[serde(rename = "vault__search")]
    Search,
    #[serde(rename = "vault__get")]
    Get,
    #[serde(rename = "vault__create")]
    Create,
    #[serde(rename = "vault__update")]
    Update,
    #[serde(rename = "vault__trash")]
    Trash,
    #[serde(rename = "vault__restore")]
    Restore,
    #[serde(rename = "vault__totp_code")]
    TotpCode,
    #[serde(rename = "vault__attachment_get")]
    AttachmentGet,
    #[serde(rename = "vault__attachment_put")]
    AttachmentPut,
    #[serde(rename = "vault__folder_list")]
    FolderList,
    #[serde(rename = "vault__folder_create")]
    FolderCreate,
    #[serde(rename = "vault__folder_update")]
    FolderUpdate,
    #[serde(rename = "vault__folder_delete")]
    FolderDelete,
}

impl VaultTool {
    /// Every tool the family exposes, in the order the plan lists them.
    pub const ALL: [Self; 13] = [
        Self::Search,
        Self::Get,
        Self::Create,
        Self::Update,
        Self::Trash,
        Self::Restore,
        Self::TotpCode,
        Self::AttachmentGet,
        Self::AttachmentPut,
        Self::FolderList,
        Self::FolderCreate,
        Self::FolderUpdate,
        Self::FolderDelete,
    ];

    /// The MCP tool name this operation answers to.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "vault__search",
            Self::Get => "vault__get",
            Self::Create => "vault__create",
            Self::Update => "vault__update",
            Self::Trash => "vault__trash",
            Self::Restore => "vault__restore",
            Self::TotpCode => "vault__totp_code",
            Self::AttachmentGet => "vault__attachment_get",
            Self::AttachmentPut => "vault__attachment_put",
            Self::FolderList => "vault__folder_list",
            Self::FolderCreate => "vault__folder_create",
            Self::FolderUpdate => "vault__folder_update",
            Self::FolderDelete => "vault__folder_delete",
        }
    }

    /// The operation a tool name names, or `None` when it is not one of the
    /// family's thirteen.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == name)
    }
}

/// One attachment a held call names, as the card draws it: the file name and
/// size, and the attachment a replace would remove. The bytes never ride the
/// draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultAttachmentDraft {
    /// The file's name, as the card draws it.
    pub name: String,
    /// The file's size in bytes, as the card draws it.
    pub size: u64,
    /// The attachment id a replace would remove, when the call replaces one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
}

/// One vault call held for the reader's approval.
///
/// The blocked MCP handler awaits the answer; no operation runs in the vault
/// unless the answer approved it. Every field here is safe to draw: no
/// secret value, no key, no TOTP seed, no card number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultDraft {
    /// The call's own id, which is what an answer is addressed by.
    pub id: Uuid,
    /// The tool the call names.
    pub tool: VaultTool,
    /// The one-line summary the card draws. Written by the facade; never a
    /// secret value.
    pub summary: String,
    /// The item the call reads or changes, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// The item's name, so the card names what it is about without a second
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_name: Option<String>,
    /// The field a reveal names, or the field set a change touches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// A diff with every secret value masked, when the call changes fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masked_diff: Option<String>,
    /// The attachment a call gets or puts, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<VaultAttachmentDraft>,
    /// Whether the shared session is locked as this card is drawn. The core
    /// fills this from the live session on every read, never at park time,
    /// so a card whose sibling unlocked first re-bases to an ordinary
    /// approve card.
    #[serde(default)]
    pub needs_unlock: bool,
}

impl VaultDraft {
    /// A draft for `tool` with the summary a card draws and no item, field,
    /// diff or attachment named yet.
    #[must_use]
    pub fn new(tool: VaultTool, summary: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            tool,
            summary: summary.into(),
            item_id: None,
            item_name: None,
            field: None,
            masked_diff: None,
            attachment: None,
            needs_unlock: false,
        }
    }

    /// The draft with the live session's lock state stamped on it.
    #[must_use]
    pub fn with_unlock_state(mut self, needs_unlock: bool) -> Self {
        self.needs_unlock = needs_unlock;
        self
    }
}

/// What the reader decided about a held vault call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum VaultAskDecision {
    /// Run the call as the card described it.
    Approve,
    /// Send the call back with a note; the agent may revise and ask again.
    SendBack { note: String },
    /// Run nothing. The blocked call returns a refusal.
    Decline,
}

/// The reader's answer to a held vault call, including the unlock material
/// when the card asked for it.
///
/// **The master password rides only this answer.** It never enters the
/// model's arguments and never reaches the transcript: the dock's masked
/// field sends it over the local answer channel, and the core hands it to
/// the unlock path and zeroizes it. The manual `Debug` redacts both it and
/// the 2FA code.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultAskAnswer {
    /// What the reader decided.
    ///
    /// **Flattened, because the decision IS the answer's tag.** The client
    /// writes `{"decision":"approve", ...}`; without this the internally
    /// tagged enum nests one level down and no answer decodes.
    #[serde(flatten)]
    pub decision: VaultAskDecision,
    /// The master password, when this card asked for it and the reader
    /// entered one. Absent on every card the session was already unlocked
    /// for, and ignored when the session unlocked first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_password: Option<String>,
    /// The 2FA code, when the account asks for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub two_factor: Option<String>,
}

impl Drop for VaultAskAnswer {
    fn drop(&mut self) {
        if let Some(password) = &mut self.master_password {
            password.zeroize();
        }
        if let Some(code) = &mut self.two_factor {
            code.zeroize();
        }
    }
}

impl std::fmt::Debug for VaultAskAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultAskAnswer")
            .field("decision", &self.decision)
            .field("master_password", &self.master_password.as_ref().map(|_| "[redacted]"))
            .field("two_factor", &self.two_factor.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// What ended a held vault call, which a view that did not answer it has no
/// other way to learn. The tool result carries the operation's own outcome;
/// this says only how the card was settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultAskEnding {
    /// The reader approved the call. The tool result reports whether the
    /// operation itself succeeded.
    Approved,
    /// The reader sent the call back with a note; nothing ran.
    SentBack,
    /// The reader declined; nothing ran.
    Declined,
    /// The asking session went away before the call was settled.
    Abandoned,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool's wire name parses back to itself, so the card and the
    /// agent's request cannot disagree about which operation is approved.
    #[test]
    fn every_tool_name_round_trips() {
        for tool in VaultTool::ALL {
            assert_eq!(VaultTool::parse(tool.as_str()), Some(tool), "{}", tool.as_str());
        }
        assert_eq!(VaultTool::parse("vault__hard_delete"), None);
    }

    /// The draft's serialised keys are exactly the safe set: adding a secret
    /// field would fail this test rather than reach a card.
    #[test]
    fn a_draft_serialises_only_what_a_card_draws() {
        let mut draft = VaultDraft::new(VaultTool::Get, "read the password for 'bank'");
        draft.item_id = Some("cipher-id".to_owned());
        draft.item_name = Some("bank".to_owned());
        draft.field = Some("password".to_owned());
        draft.masked_diff = Some("password changed".to_owned());
        draft.attachment =
            Some(VaultAttachmentDraft { name: "notes.txt".to_owned(), size: 97, replaces: None });
        let value = serde_json::to_value(&draft).expect("a draft serialises");
        let object = value.as_object().expect("a draft serialises as an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "attachment",
                "field",
                "id",
                "item_id",
                "item_name",
                "masked_diff",
                "needs_unlock",
                "summary",
                "tool"
            ],
        );
    }

    /// The serde spelling and `as_str` are one name, not two that happen to
    /// round-trip through `parse`: the wire name is what the card and the
    /// audit line carry.
    #[test]
    fn every_tool_serde_name_is_its_as_str() {
        for tool in VaultTool::ALL {
            let value = serde_json::to_value(tool).expect("a tool encodes");
            assert_eq!(value.as_str(), Some(tool.as_str()), "{}", tool.as_str());
        }
    }

    /// A draft and an answer round-trip through serde, which is what the
    /// socket does with both.
    #[test]
    fn the_ask_shapes_round_trip() {
        let mut draft = VaultDraft::new(VaultTool::AttachmentPut, "upload 'notes.txt' (97 bytes)");
        draft.item_id = Some("cipher-id".to_owned());
        draft.item_name = Some("bank".to_owned());
        draft.attachment = Some(VaultAttachmentDraft {
            name: "notes.txt".to_owned(),
            size: 97,
            replaces: Some("old-attachment".to_owned()),
        });
        let json = serde_json::to_string(&draft).expect("serialise");
        let back: VaultDraft = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(draft, back);

        let answer = VaultAskAnswer {
            decision: VaultAskDecision::SendBack { note: "trim the summary".to_owned() },
            master_password: Some("never printed".to_owned()),
            two_factor: Some("123456".to_owned()),
        };
        let json = serde_json::to_string(&answer).expect("serialise");
        let back: VaultAskAnswer = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(answer, back);
        assert!(!format!("{answer:?}").contains("never printed"));
        assert!(!format!("{answer:?}").contains("123456"));
    }

    /// The lock state is stamped per read, so a draft parked while locked
    /// can be re-based by the core without being rebuilt.
    #[test]
    fn the_unlock_state_is_stamped_not_frozen() {
        let draft = VaultDraft::new(VaultTool::Search, "find 'bank'").with_unlock_state(true);
        assert!(draft.needs_unlock);
        let rebased = draft.with_unlock_state(false);
        assert!(!rebased.needs_unlock);
    }
}
