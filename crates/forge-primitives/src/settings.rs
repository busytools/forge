//! The effective-config document a server pushes to every client.
//!
//! One document is built per load and shared by every connection: the areas
//! `forge.toml` is read into, every key's effective value, and the class that
//! says when an edit to that key takes effect. Values cross as the file loads
//! them, unredacted - the socket is reached over loopback or the private
//! network, which is the boundary.

use serde::{Deserialize, Serialize};

/// When an edit to a key takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Read on the key's next use.
    Live,
    /// Read when a session or a connection opens.
    NextSession,
    /// Built once at boot; an edit needs a restart.
    Restart,
    /// The loader accepts the key and nothing reads it yet.
    NotReadYet,
}

impl EffectClass {
    /// The word the page draws and the class catalogue's own name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::NextSession => "next session",
            Self::Restart => "restart",
            Self::NotReadYet => "not read yet",
        }
    }
}

/// The shape of a key's value, so a client picks a drawing without guessing
/// from the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingKind {
    String,
    Number,
    Bool,
    List,
    Table,
    Set,
}

/// One area of the config, as the page's sub-nav lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsArea {
    pub id: String,
    pub name: String,
    pub note: String,
    pub keys: Vec<SettingsKey>,
}

/// One key's effective value and the class that says when a change lands.
///
/// Hand-written `Debug`: a key's value can be an account token, and the frame
/// is the page's read rather than a diagnostic. The class and path stay
/// legible; the value never prints.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsKey {
    /// The concrete key path, with the instance where the table repeats:
    /// `server.port`, `accounts.Subspace.token`.
    pub path: String,
    /// The key's own name as a row draws it: the catalogue key's leaf, or
    /// the slug or flag a repeated key names. A name may contain dots, so
    /// a reader cannot cut it from `path`.
    pub leaf: String,
    /// The instance names the row belongs to, from the table's repeat keys:
    /// an account, an org and a project, a Slack table and workspace, a
    /// model, a server table. Empty when the area's keys do not repeat.
    pub group: Vec<String>,
    pub kind: SettingKind,
    /// In the shape `kind` names: a JSON string, number, bool, array of
    /// strings, or object of string values.
    pub value: serde_json::Value,
    pub class: EffectClass,
}

impl std::fmt::Debug for SettingsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsKey")
            .field("path", &self.path)
            .field("leaf", &self.leaf)
            .field("group", &self.group)
            .field("kind", &self.kind)
            .field("value", &"[redacted]")
            .field("class", &self.class)
            .finish()
    }
}

/// The header's own totals, built with the document so a client does not
/// count for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsTotals {
    pub keys: usize,
    pub areas: usize,
}

/// The whole effective config, pushed on attach and after every applied
/// reload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsDocument {
    /// The forge version that built it.
    pub version: String,
    /// The resolved `forge.toml` path.
    pub source: String,
    pub totals: SettingsTotals,
    pub areas: Vec<SettingsArea>,
}

impl SettingsDocument {
    /// A fresh total over the areas, so a builder cannot ship a count that
    /// disagrees with what it drew.
    pub fn with_totals(mut self) -> Self {
        self.totals = SettingsTotals {
            keys: self.areas.iter().map(|area| area.keys.len()).sum(),
            areas: self.areas.len(),
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame is the page's read, not a diagnostic: a token that rides in
    /// it must not print through a debug log.
    #[test]
    fn a_keys_debug_never_prints_its_value() {
        let key = SettingsKey {
            path: "accounts.Primary.token".to_owned(),
            leaf: "token".to_owned(),
            group: vec!["Primary".to_owned()],
            kind: SettingKind::String,
            value: serde_json::json!("sk-a-real-token"),
            class: EffectClass::Live,
        };
        let shown = format!("{key:?}");
        assert!(!shown.contains("sk-a-real-token"), "the value printed: {shown}");
        assert!(shown.contains("accounts.Primary.token"), "the path stayed legible: {shown}");
    }
}
