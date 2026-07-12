//! Subagent declarations forwarded to the `claude` CLI via the
//! `initialize` `control_request`'s `agents` field. Pure data - no
//! callbacks.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::options::PermissionMode;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDefinition {
    /// Short description shown to the main agent when it picks a subagent.
    pub description: String,
    /// System prompt the subagent runs with.
    pub prompt: String,
    /// Tool allowlist for this subagent. `None` = no additional restriction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Tool denylist for this subagent. Wire key is `disallowedTools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    /// Model alias (`"sonnet"`, `"opus"`, `"haiku"`, `"inherit"`) or a
    /// full model ID. `None` = inherit the main agent's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Concrete skills this subagent should load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Which `CLAUDE.md` scope to surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<SubagentMemory>,
    /// MCP servers available to this subagent. Each entry is either a named
    /// reference to a top-level server or an inline `{name: config}`
    /// definition. Wire key is `mcpServers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<SubagentMcpServerRef>>,
    /// Seed turn injected at subagent start. Wire key is `initialPrompt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    /// Upper bound on turns this subagent may take. Wire key is `maxTurns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u64>,
    /// Run the subagent without surfacing its intermediate turns in the
    /// parent transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// Reasoning-effort hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<SubagentEffort>,
    /// Override permission-mode for this subagent only. Wire key is
    /// `permissionMode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
}

// Populate `SubagentDefinition` fields directly. A builder family
// (`::new` / `with_*`) can be added when subagent registration
// becomes a runtime feature with real callers.

/// `CLAUDE.md` scope surfaced to a subagent. Wire shape:
/// `Literal["user", "project", "local"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentMemory {
    /// User-scope (`<config_dir>/CLAUDE.md`).
    User,
    /// Project-scope (`<repo>/CLAUDE.md`).
    Project,
    /// Project-local (`<repo>/.claude/CLAUDE.md.local`).
    Local,
}

/// One entry in [`SubagentDefinition::mcp_servers`]. Either the name of an MCP
/// server configured at the top level, or an inline `{name: config}` object.
/// Matches the CLI's `list[str | dict[str, Any]]` type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubagentMcpServerRef {
    /// Named reference to a top-level MCP server.
    Named(String),
    /// Inline declaration. On the wire, emitted as `{<name>: <config>}`.
    #[serde(with = "inline_mcp_server")]
    Inline {
        /// Server name - becomes the single key of the emitted object.
        name: String,
        /// Server config - becomes the value.
        config: Value,
    },
}

mod inline_mcp_server {
    use serde::de::{Error as _, MapAccess, Visitor};
    use serde::ser::SerializeMap;
    use serde_json::Value;
    use std::fmt;

    pub(super) fn serialize<S: serde::Serializer>(
        name: &str,
        config: &Value,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(name, config)?;
        map.end()
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<(String, Value), D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = (String, Value);
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a single-key {name: config} object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let first: Option<(String, Value)> = map.next_entry()?;
                let (name, config) =
                    first.ok_or_else(|| M::Error::custom("empty inline MCP server object"))?;
                if map.next_entry::<String, Value>()?.is_some() {
                    return Err(M::Error::custom(
                        "inline MCP server object must have exactly one key",
                    ));
                }
                Ok((name, config))
            }
        }
        d.deserialize_map(V)
    }
}

/// Reasoning-effort hint on a subagent. Wire shape:
/// `Literal["low","medium","high","max"] | int`. Named
/// `SubagentEffort` to disambiguate from the runtime model effort
/// in `forge_primitives::runtime::EffortLevel`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubagentEffort {
    /// One of the named presets.
    Preset(EffortPreset),
    /// Numeric override (CLI-defined semantics).
    Numeric(i64),
}

impl SubagentEffort {
    /// String form suitable for passing via `--effort <value>` or
    /// any other CLI surface that expects a literal-or-int.
    pub fn as_cli_arg(&self) -> String {
        match self {
            Self::Preset(p) => match p {
                EffortPreset::Low => "low".into(),
                EffortPreset::Medium => "medium".into(),
                EffortPreset::High => "high".into(),
                EffortPreset::Max => "max".into(),
            },
            Self::Numeric(n) => n.to_string(),
        }
    }
}

/// Named reasoning-effort presets. Wire shape:
/// `Literal["low","medium","high","max"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortPreset {
    /// Lowest effort - fast, shallow reasoning.
    Low,
    /// Balanced default.
    Medium,
    /// Deeper reasoning.
    High,
    /// Maximum reasoning (may be slow).
    Max,
}
