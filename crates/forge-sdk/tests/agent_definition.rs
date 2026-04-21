//! Wire-parity tests for `AgentDefinition` + the `agents` option.
//!
//! Ported from claude-agent-sdk-python v0.1.64 `types.py:82-99` +
//! `_internal/client.py:153-159` (the `{k: v for k, v in asdict(def).items()
//! if v is not None}` filter that drops unset fields from the initialize
//! payload).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::agents::{
    AgentDefinition, AgentMcpServerRef, AgentMemory, EffortLevel, EffortPreset,
};
use forge_sdk::{OptionsBuilder, PermissionMode};
use serde_json::json;

#[test]
fn minimal_definition_serialises_to_description_and_prompt_only() {
    let def = AgentDefinition::new("A helpful subagent", "You are a task runner.");
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
    let def = AgentDefinition::new("d", "p")
        .with_tools(vec!["Edit".into(), "Read".into()])
        .with_disallowed_tools(vec!["Bash".into()])
        .with_model("claude-opus-4-7")
        .with_skills(vec!["web-search".into()])
        .with_memory(AgentMemory::Project)
        .with_mcp_servers(vec![AgentMcpServerRef::Named("github".into())])
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
    let back: AgentDefinition = serde_json::from_value(v).expect("de");
    assert_eq!(back, def);
}

#[test]
fn effort_accepts_numeric_value() {
    let def = AgentDefinition::new("d", "p").with_effort(EffortLevel::Numeric(42));
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
        (AgentMemory::User, "user"),
        (AgentMemory::Project, "project"),
        (AgentMemory::Local, "local"),
    ] {
        assert_eq!(serde_json::to_value(m).unwrap(), json!(wire));
    }
}

#[test]
fn inline_mcp_server_serialises_as_single_key_object() {
    let def = AgentDefinition::new("d", "p").with_mcp_servers(vec![AgentMcpServerRef::Inline {
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
    let def = AgentDefinition::new("d", "p").with_max_turns(5);
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
fn options_builder_agent_registers_under_name() {
    let options = OptionsBuilder::new()
        .agent(
            "researcher",
            AgentDefinition::new("Researches a topic", "You search and summarise."),
        )
        .agent(
            "coder",
            AgentDefinition::new("Writes code", "You write Rust."),
        )
        .build();
    assert_eq!(options.agents.len(), 2);
    assert!(options.agents.contains_key("researcher"));
    assert!(options.agents.contains_key("coder"));
}
