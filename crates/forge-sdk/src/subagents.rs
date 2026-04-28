//! [`SubagentDefinition`] and the nested types it carries.
//!
//! Forwarded to the CLI via the `initialize` `control_request`'s
//! `agents` field when [`Options::subagents`](crate::options::Options::subagents)
//! is non-empty. Unset fields are skipped on the wire via
//! `skip_serializing_if = "Option::is_none"` so the JSON shape matches
//! what the CLI expects.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::options::PermissionMode;

/// One subagent declaration. Passed by name in the initialize payload so
/// the `claude` binary can spawn it on `Task(description, subagent=<name>)`.
///
/// Construct via [`SubagentDefinition::new`] + the `with_*` fluent setters —
/// the struct is `#[non_exhaustive]` so future parity drops can land
/// without breaking callers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
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
    pub effort: Option<EffortLevel>,
    /// Override permission-mode for this subagent only. Wire key is
    /// `permissionMode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
}

impl SubagentDefinition {
    /// Construct the minimum-viable definition (description + prompt only).
    #[must_use]
    pub fn new(description: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            prompt: prompt.into(),
            tools: None,
            disallowed_tools: None,
            model: None,
            skills: None,
            memory: None,
            mcp_servers: None,
            initial_prompt: None,
            max_turns: None,
            background: None,
            effort: None,
            permission_mode: None,
        }
    }

    /// Attach a tool allowlist.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Attach a tool denylist.
    #[must_use]
    pub fn with_disallowed_tools(mut self, tools: Vec<String>) -> Self {
        self.disallowed_tools = Some(tools);
        self
    }

    /// Pin to a specific model (or alias).
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Attach a skill list.
    #[must_use]
    pub fn with_skills(mut self, skills: Vec<String>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Select which `CLAUDE.md` scope to surface.
    #[must_use]
    pub fn with_memory(mut self, memory: SubagentMemory) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Attach the MCP servers this subagent should see.
    #[must_use]
    pub fn with_mcp_servers(mut self, servers: Vec<SubagentMcpServerRef>) -> Self {
        self.mcp_servers = Some(servers);
        self
    }

    /// Set a seed turn.
    #[must_use]
    pub fn with_initial_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.initial_prompt = Some(prompt.into());
        self
    }

    /// Cap the turns this subagent may take.
    #[must_use]
    pub fn with_max_turns(mut self, n: u64) -> Self {
        self.max_turns = Some(n);
        self
    }

    /// Toggle background mode.
    #[must_use]
    pub fn with_background(mut self, background: bool) -> Self {
        self.background = Some(background);
        self
    }

    /// Set the reasoning-effort hint.
    #[must_use]
    pub fn with_effort(mut self, effort: EffortLevel) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Override permission-mode for this subagent only.
    #[must_use]
    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = Some(mode);
        self
    }
}

/// `CLAUDE.md` scope surfaced to a subagent. Wire shape:
/// `Literal["user", "project", "local"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentMemory {
    /// User-scope (`~/.claude/CLAUDE.md`).
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
        /// Server name — becomes the single key of the emitted object.
        name: String,
        /// Server config — becomes the value.
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
/// `Literal["low","medium","high","max"] | int`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffortLevel {
    /// One of the named presets.
    Preset(EffortPreset),
    /// Numeric override (CLI-defined semantics).
    Numeric(i64),
}

impl EffortLevel {
    /// String form suitable for passing via `--effort <value>` or
    /// any other CLI surface that expects a literal-or-int.
    #[must_use]
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
    /// Lowest effort — fast, shallow reasoning.
    Low,
    /// Balanced default.
    Medium,
    /// Deeper reasoning.
    High,
    /// Maximum reasoning (may be slow).
    Max,
}

/// Map of subagent-name → [`SubagentDefinition`] attached to
/// [`Options`](crate::options::Options). Empty by default.
pub type SubagentMap = HashMap<String, SubagentDefinition>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    use crate::{OptionsBuilder, PermissionMode};
    use serde_json::json;

    #[test]
    fn minimal_definition_serialises_to_description_and_prompt_only() {
        let def = SubagentDefinition::new("A helpful subagent", "You are a task runner.");
        let v = serde_json::to_value(&def).expect("ser");
        assert_eq!(
            v,
            json!({
                "description": "A helpful subagent",
                "prompt": "You are a task runner."
            })
        );
    }

    #[test]
    fn full_definition_round_trips_with_camel_case_keys() {
        let def = SubagentDefinition::new("d", "p")
            .with_tools(vec!["Edit".into(), "Read".into()])
            .with_disallowed_tools(vec!["Bash".into()])
            .with_model("claude-opus-4-7")
            .with_skills(vec!["web-search".into()])
            .with_memory(SubagentMemory::Project)
            .with_mcp_servers(vec![SubagentMcpServerRef::Named("github".into())])
            .with_initial_prompt("hello world")
            .with_max_turns(25)
            .with_background(true)
            .with_effort(EffortLevel::Preset(EffortPreset::High))
            .with_permission_mode(PermissionMode::AcceptEdits);
        let v = serde_json::to_value(&def).expect("ser");
        assert_eq!(v["description"], "d");
        assert_eq!(v["prompt"], "p");
        assert_eq!(v["tools"], json!(["Edit", "Read"]));
        assert_eq!(v["disallowedTools"], json!(["Bash"]));
        assert_eq!(v["model"], "claude-opus-4-7");
        assert_eq!(v["skills"], json!(["web-search"]));
        assert_eq!(v["memory"], "project");
        assert_eq!(v["mcpServers"], json!(["github"]));
        assert_eq!(v["initialPrompt"], "hello world");
        assert_eq!(v["maxTurns"], 25);
        assert_eq!(v["background"], true);
        assert_eq!(v["effort"], "high");
        assert_eq!(v["permissionMode"], "acceptEdits");
        let back: SubagentDefinition = serde_json::from_value(v).expect("de");
        assert_eq!(back, def);
    }

    #[test]
    fn effort_accepts_numeric_value() {
        let def = SubagentDefinition::new("d", "p").with_effort(EffortLevel::Numeric(42));
        let v = serde_json::to_value(&def).expect("ser");
        assert_eq!(v["effort"], 42);
    }

    #[test]
    fn effort_preset_values_match_python_literal() {
        for (p, wire) in [
            (EffortPreset::Low, "low"),
            (EffortPreset::Medium, "medium"),
            (EffortPreset::High, "high"),
            (EffortPreset::Max, "max"),
        ] {
            assert_eq!(serde_json::to_value(p).unwrap(), json!(wire));
        }
    }

    #[test]
    fn memory_values_match_python_literal() {
        for (m, wire) in [
            (SubagentMemory::User, "user"),
            (SubagentMemory::Project, "project"),
            (SubagentMemory::Local, "local"),
        ] {
            assert_eq!(serde_json::to_value(m).unwrap(), json!(wire));
        }
    }

    #[test]
    fn inline_mcp_server_serialises_as_single_key_object() {
        let def =
            SubagentDefinition::new("d", "p").with_mcp_servers(vec![SubagentMcpServerRef::Inline {
                name: "local".into(),
                config: json!({"type": "stdio", "command": "my-server"}),
            }]);
        let v = serde_json::to_value(&def).expect("ser");
        assert_eq!(
            v["mcpServers"],
            json!([{"local": {"type": "stdio", "command": "my-server"}}])
        );
    }

    #[test]
    fn none_fields_are_omitted_from_wire_matching_python_asdict_filter() {
        let def = SubagentDefinition::new("d", "p").with_max_turns(5);
        let v = serde_json::to_value(&def).expect("ser");
        let obj = v.as_object().expect("object");
        assert!(obj.contains_key("description"));
        assert!(obj.contains_key("prompt"));
        assert!(obj.contains_key("maxTurns"));
        assert!(!obj.contains_key("tools"));
        assert!(!obj.contains_key("disallowedTools"));
        assert!(!obj.contains_key("permissionMode"));
        assert!(!obj.contains_key("effort"));
    }

    #[test]
    fn options_builder_subagent_registers_under_name() {
        let options = OptionsBuilder::new()
            .subagent(
                "researcher",
                SubagentDefinition::new("Researches a topic", "You search and summarise."),
            )
            .subagent(
                "coder",
                SubagentDefinition::new("Writes code", "You write Rust."),
            )
            .build();
        assert_eq!(options.subagents.len(), 2);
        assert!(options.subagents.contains_key("researcher"));
        assert!(options.subagents.contains_key("coder"));
    }
}
