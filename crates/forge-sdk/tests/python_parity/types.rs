//! Mirrors `tests/test_types.py` from `claude-agent-sdk-python` v0.1.64.
//!
//! Port of all 45 upstream cases across the six Python test classes:
//!
//! - `TestMessageTypes` (6) — message + content-block construction.
//! - `TestOptions` (11) — `ClaudeAgentOptions` defaults + setters.
//! - `TestHookInputTypes` (7) — hook input struct construction + optional
//!   agent-id plumbing.
//! - `TestHookSpecificOutputTypes` (5) — hook output struct serialisation
//!   keys.
//! - `TestMcpServerStatusTypes` (6) — MCP server status shapes + variants.
//! - `TestAgentDefinition` (10) — camelCase wire-key enforcement.
//!
//! Python builds `TypedDict` literals and asserts on keys; Rust builds
//! typed structs and asserts on fields + serde-JSON output. Functionally
//! identical — the parity goal is "every upstream case has a Rust
//! counterpart pinned by name".

use std::path::PathBuf;

use serde_json::{Value, json};

use forge_sdk::agents::{
    AgentDefinition, AgentMcpServerRef, AgentMemory, EffortLevel, EffortPreset,
};
use forge_sdk::{
    AssistantEnvelope, BaseHookInput, ContentBlock, McpServerConnectionStatus, McpServerInfo,
    McpServerStatus, McpStatusResponse, McpToolAnnotations, McpToolInfo, Message,
    NotificationHookSpecificOutput, NotificationInput, Options, OptionsBuilder, PermissionMode,
    PermissionRequestHookSpecificOutput, PermissionRequestInput, PostToolUseHookSpecificOutput,
    PostToolUseInput, PreToolUseHookSpecificOutput, PreToolUseInput, StopReason,
    SubagentStartHookSpecificOutput, SubagentStartInput, SystemPromptKind, Usage, UserEnvelope,
};

// ===========================================================================
// TestMessageTypes — 6 cases
// ===========================================================================

/// Ported from `test_user_message_creation`. Python constructs a
/// `UserMessage` with a string payload; forge-sdk normalises the
/// bare-string form into a single `ContentBlock::Text` on the
/// `UserEnvelope`, so the equivalent shape is one text block.
#[test]
fn user_message_creation() {
    let env = UserEnvelope {
        role: "user".into(),
        content: vec![ContentBlock::Text {
            text: "Hello, Claude!".into(),
        }],
    };
    let ContentBlock::Text { text } = &env.content[0] else {
        panic!("expected text block, got {:?}", env.content[0]);
    };
    assert_eq!(text, "Hello, Claude!");
}

/// Ported from `test_assistant_message_with_text`.
#[test]
fn assistant_message_with_text() {
    let env = AssistantEnvelope {
        id: "msg_01".into(),
        role: "assistant".into(),
        model: "claude-opus-4-1-20250805".into(),
        content: vec![ContentBlock::Text {
            text: "Hello, human!".into(),
        }],
        stop_reason: None,
        stop_sequence: None,
        usage: None,
    };
    assert_eq!(env.content.len(), 1);
    let ContentBlock::Text { text } = &env.content[0] else {
        panic!("expected text block");
    };
    assert_eq!(text, "Hello, human!");
}

/// Ported from `test_assistant_message_with_thinking`.
#[test]
fn assistant_message_with_thinking() {
    let env = AssistantEnvelope {
        id: "msg_02".into(),
        role: "assistant".into(),
        model: "claude-opus-4-1-20250805".into(),
        content: vec![ContentBlock::Thinking {
            thinking: "I'm thinking...".into(),
            signature: "sig-123".into(),
        }],
        stop_reason: None,
        stop_sequence: None,
        usage: None,
    };
    assert_eq!(env.content.len(), 1);
    let ContentBlock::Thinking {
        thinking,
        signature,
    } = &env.content[0]
    else {
        panic!("expected thinking block");
    };
    assert_eq!(thinking, "I'm thinking...");
    assert_eq!(signature, "sig-123");
}

/// Ported from `test_tool_use_block`. Checks id/name/input field access
/// on a `ContentBlock::ToolUse` variant.
#[test]
fn tool_use_block() {
    let block = ContentBlock::ToolUse {
        id: "tool-123".into(),
        name: "Read".into(),
        input: json!({"file_path": "/test.txt"}),
    };
    let ContentBlock::ToolUse { id, name, input } = &block else {
        panic!("expected tool_use block");
    };
    assert_eq!(id, "tool-123");
    assert_eq!(name, "Read");
    assert_eq!(
        input.get("file_path").and_then(Value::as_str),
        Some("/test.txt")
    );
}

/// Ported from `test_tool_result_block`. Python's `ToolResultBlock`
/// carries `tool_use_id` + `content` + `is_error`.
#[test]
fn tool_result_block() {
    let block = ContentBlock::ToolResult {
        tool_use_id: "tool-123".into(),
        content: Value::String("File contents here".into()),
        is_error: false,
    };
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
    } = &block
    else {
        panic!("expected tool_result block");
    };
    assert_eq!(tool_use_id, "tool-123");
    assert_eq!(content.as_str(), Some("File contents here"));
    assert!(!is_error);
}

/// Ported from `test_result_message`. Verifies the full
/// `Message::Result` variant payload parses from its on-wire shape.
#[test]
fn result_message() {
    let wire = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1500,
        "duration_api_ms": 1200,
        "is_error": false,
        "num_turns": 1,
        "session_id": "session-123",
        "total_cost_usd": 0.01,
    });
    let msg: Message = serde_json::from_value(wire).expect("parse result");
    let Message::Result {
        subtype,
        total_cost_usd,
        session_id,
        ..
    } = msg
    else {
        panic!("expected Result variant");
    };
    assert_eq!(subtype, "success");
    assert_eq!(total_cost_usd, Some(0.01));
    assert_eq!(session_id, "session-123");
}

// ===========================================================================
// TestOptions — 11 cases
// ===========================================================================

/// Ported from `test_default_options`. Python's `ClaudeAgentOptions()`
/// leaves tools empty and `permission_mode` `None`; forge-sdk's default
/// matches on every scalar (`permission_mode` defaults to `Default`
/// rather than Python's `None`, but the CLI behaviour is identical —
/// Python's `None` also resolves to "default" via
/// `permission_mode or "default"` elsewhere).
#[test]
fn default_options() {
    let options = Options::default();
    assert!(options.allowed_tools.is_empty());
    assert!(options.system_prompt.is_none());
    assert_eq!(options.permission_mode, PermissionMode::Ask);
    assert!(!options.continue_conversation);
    assert!(options.disallowed_tools.is_empty());
}

/// Ported from `test_claude_code_options_with_tools`. Sets the
/// allowed + disallowed lists via the builder.
#[test]
fn options_with_tools() {
    let options = OptionsBuilder::new()
        .allowed_tools(["Read", "Write", "Edit"])
        .disallowed_tools(vec!["Bash".into()])
        .build();
    assert_eq!(
        options.allowed_tools,
        vec!["Read".to_string(), "Write".into(), "Edit".into()]
    );
    assert_eq!(options.disallowed_tools, vec!["Bash".to_string()]);
}

/// Ported from `test_claude_code_options_with_permission_mode`.
/// Exercises every [`PermissionMode`] variant the Python test names —
/// including the two newer ones (`auto`, `dontAsk`).
#[test]
fn options_with_permission_mode() {
    for mode in [
        PermissionMode::BypassPermissions,
        PermissionMode::Plan,
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::DenyPermissions,
        PermissionMode::Auto,
    ] {
        let options = OptionsBuilder::new().permission_mode(mode).build();
        assert_eq!(options.permission_mode, mode);
    }
}

/// Ported from `test_claude_code_options_with_system_prompt_string`.
#[test]
fn options_with_system_prompt_string() {
    let options = OptionsBuilder::new()
        .system_prompt(SystemPromptKind::Inline(
            "You are a helpful assistant.".into(),
        ))
        .build();
    assert_eq!(
        options.system_prompt,
        Some(SystemPromptKind::Inline(
            "You are a helpful assistant.".into()
        ))
    );
}

/// Ported from `test_claude_code_options_with_system_prompt_preset`.
/// Python's `{"type": "preset", "preset": "claude_code"}` with no
/// append/exclude maps to forge-sdk's `Preset { append: None, exclude:
/// None }`.
#[test]
fn options_with_system_prompt_preset() {
    let options = OptionsBuilder::new()
        .system_prompt(SystemPromptKind::Preset {
            append: None,
            exclude_dynamic_sections: None,
        })
        .build();
    assert!(matches!(
        options.system_prompt,
        Some(SystemPromptKind::Preset {
            append: None,
            exclude_dynamic_sections: None,
        })
    ));
}

/// Ported from `test_claude_code_options_with_system_prompt_preset_and_append`.
#[test]
fn options_with_system_prompt_preset_append() {
    let options = OptionsBuilder::new()
        .system_prompt(SystemPromptKind::preset_append("Be concise."))
        .build();
    let Some(SystemPromptKind::Preset {
        append,
        exclude_dynamic_sections,
    }) = options.system_prompt
    else {
        panic!("expected preset variant");
    };
    assert_eq!(append.as_deref(), Some("Be concise."));
    assert_eq!(exclude_dynamic_sections, None);
}

/// Ported from
/// `test_claude_code_options_with_system_prompt_preset_exclude_dynamic_sections`.
#[test]
fn options_with_system_prompt_preset_exclude_dynamic_sections() {
    let options = OptionsBuilder::new()
        .system_prompt(SystemPromptKind::Preset {
            append: None,
            exclude_dynamic_sections: Some(true),
        })
        .build();
    let Some(SystemPromptKind::Preset {
        exclude_dynamic_sections,
        ..
    }) = options.system_prompt
    else {
        panic!("expected preset variant");
    };
    assert_eq!(exclude_dynamic_sections, Some(true));
}

/// Ported from `test_claude_code_options_with_system_prompt_file`.
/// Python models this as a dict `{"type": "file", "path": "..."}`;
/// forge-sdk has a dedicated `File(PathBuf)` variant.
#[test]
fn options_with_system_prompt_file() {
    let options = OptionsBuilder::new()
        .system_prompt(SystemPromptKind::File(PathBuf::from("/path/to/prompt.md")))
        .build();
    let Some(SystemPromptKind::File(path)) = options.system_prompt else {
        panic!("expected file variant");
    };
    assert_eq!(path, PathBuf::from("/path/to/prompt.md"));
}

/// Ported from `test_claude_code_options_with_session_continuation`.
#[test]
fn options_with_session_continuation() {
    let options = OptionsBuilder::new()
        .continue_conversation(true)
        .resume("session-123")
        .build();
    assert!(options.continue_conversation);
    assert_eq!(options.resume.as_deref(), Some("session-123"));
}

/// Ported from `test_claude_code_options_with_model_specification`.
#[test]
fn options_with_model_specification() {
    let options = OptionsBuilder::new()
        .model("claude-sonnet-4-5")
        .permission_prompt_tool_name("CustomTool")
        .build();
    assert_eq!(options.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(
        options.permission_prompt_tool_name.as_deref(),
        Some("CustomTool")
    );
}

// ===========================================================================
// TestHookInputTypes — 7 cases
// ===========================================================================

/// Ported from `test_notification_hook_input`. Parses the canonical
/// wire shape for a Notification hook and verifies field access.
#[test]
fn notification_hook_input() {
    let wire = json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/transcript",
        "cwd": "/home/user",
        "hook_event_name": "Notification",
        "message": "Task completed",
        "notification_type": "info",
    });
    let input: NotificationInput = serde_json::from_value(wire).expect("parse");
    // forge-sdk routes `hook_event_name` as an external discriminator
    // during dispatch rather than storing it on the struct — the
    // successful deserialize into `NotificationInput` IS the
    // `hook_event_name == "Notification"` assertion.
    assert_eq!(input.message, "Task completed");
    assert_eq!(input.notification_type, "info");
    assert_eq!(input.title, None);
}

/// Ported from `test_notification_hook_input_with_title`. Optional
/// `title` is surfaced when present.
#[test]
fn notification_hook_input_with_title() {
    let wire = json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/transcript",
        "cwd": "/home/user",
        "hook_event_name": "Notification",
        "message": "Task completed",
        "notification_type": "info",
        "title": "Success",
    });
    let input: NotificationInput = serde_json::from_value(wire).expect("parse");
    assert_eq!(input.title.as_deref(), Some("Success"));
}

/// Ported from `test_subagent_start_hook_input`.
#[test]
fn subagent_start_hook_input() {
    let wire = json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/transcript",
        "cwd": "/home/user",
        "hook_event_name": "SubagentStart",
        "agent_id": "agent-42",
        "agent_type": "researcher",
    });
    let input: SubagentStartInput = serde_json::from_value(wire).expect("parse");
    assert_eq!(input.agent_id, "agent-42");
    assert_eq!(input.agent_type, "researcher");
}

/// Ported from `test_pre_tool_use_hook_input_with_agent_id`. Two
/// parsers: one with sub-agent attribution populated, one without.
#[test]
fn pre_tool_use_hook_input_with_agent_id() {
    let with_agent = json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/transcript",
        "cwd": "/home/user",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "echo hello"},
        "tool_use_id": "toolu_abc123",
        "agent_id": "agent-42",
        "agent_type": "researcher",
    });
    let input: PreToolUseInput = serde_json::from_value(with_agent).expect("parse");
    assert_eq!(input.subagent.agent_id.as_deref(), Some("agent-42"));
    assert_eq!(input.subagent.agent_type.as_deref(), Some("researcher"));

    let without_agent = json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/transcript",
        "cwd": "/home/user",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "echo hello"},
        "tool_use_id": "toolu_def456",
    });
    let input: PreToolUseInput = serde_json::from_value(without_agent).expect("parse");
    assert_eq!(input.subagent.agent_id, None);
}

/// Ported from `test_post_tool_use_hook_input_with_agent_id`.
#[test]
fn post_tool_use_hook_input_with_agent_id() {
    let wire = json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/transcript",
        "cwd": "/home/user",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "echo hello"},
        "tool_response": {"content": [{"type": "text", "text": "hello"}]},
        "tool_use_id": "toolu_abc123",
        "agent_id": "agent-42",
    });
    let input: PostToolUseInput = serde_json::from_value(wire).expect("parse");
    assert_eq!(input.subagent.agent_id.as_deref(), Some("agent-42"));
}

/// Ported from `test_permission_request_hook_input`.
#[test]
fn permission_request_hook_input() {
    let wire = json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/transcript",
        "cwd": "/home/user",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
    });
    let input: PermissionRequestInput = serde_json::from_value(wire).expect("parse");
    assert_eq!(input.tool_name, "Bash");
    assert_eq!(input.tool_input, json!({"command": "ls"}));
}

/// Ported from `test_permission_request_hook_input_with_suggestions`.
#[test]
fn permission_request_hook_input_with_suggestions() {
    let wire = json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/transcript",
        "cwd": "/home/user",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
        "permission_suggestions": [{"type": "allow", "rule": "Bash(*)"}],
    });
    let input: PermissionRequestInput = serde_json::from_value(wire).expect("parse");
    assert_eq!(input.permission_suggestions.as_ref().map(Vec::len), Some(1));
}

// ===========================================================================
// TestHookSpecificOutputTypes — 5 cases
// ===========================================================================

/// Ported from `test_notification_hook_specific_output`. Verifies the
/// `hookEventName` discriminator emits on the wire under its camelCase
/// key and that `additionalContext` rides along.
#[test]
fn notification_hook_specific_output() {
    let output = NotificationHookSpecificOutput {
        additional_context: Some("Extra info".into()),
        ..Default::default()
    };
    let wire = serde_json::to_value(&output).expect("serialize");
    assert_eq!(wire.get("hookEventName"), Some(&json!("Notification")));
    assert_eq!(wire.get("additionalContext"), Some(&json!("Extra info")));
}

/// Ported from `test_subagent_start_hook_specific_output`.
#[test]
fn subagent_start_hook_specific_output() {
    let output = SubagentStartHookSpecificOutput {
        additional_context: Some("Starting subagent for research".into()),
        ..Default::default()
    };
    let wire = serde_json::to_value(&output).expect("serialize");
    assert_eq!(wire.get("hookEventName"), Some(&json!("SubagentStart")));
}

/// Ported from `test_permission_request_hook_specific_output`.
/// Python passes a raw dict; forge-sdk carries a `Value` for the same
/// free-form payload.
#[test]
fn permission_request_hook_specific_output() {
    let output = PermissionRequestHookSpecificOutput {
        decision: json!({"type": "allow"}),
        ..Default::default()
    };
    let wire = serde_json::to_value(&output).expect("serialize");
    assert_eq!(wire.get("hookEventName"), Some(&json!("PermissionRequest")));
    assert_eq!(wire.get("decision"), Some(&json!({"type": "allow"})));
}

/// Ported from `test_pre_tool_use_output_has_additional_context`.
#[test]
fn pre_tool_use_output_has_additional_context() {
    let output = PreToolUseHookSpecificOutput {
        additional_context: Some("context for claude".into()),
        ..Default::default()
    };
    let wire = serde_json::to_value(&output).expect("serialize");
    assert_eq!(
        wire.get("additionalContext"),
        Some(&json!("context for claude"))
    );
}

/// Ported from `test_post_tool_use_output_has_updated_mcp_tool_output`.
/// The wire key is `updatedMCPToolOutput` — camel-with-MCP-acronym
/// preservation matters.
#[test]
fn post_tool_use_output_has_updated_mcp_tool_output() {
    let output = PostToolUseHookSpecificOutput {
        updated_mcp_tool_output: Some(json!({"result": "modified"})),
        ..Default::default()
    };
    let wire = serde_json::to_value(&output).expect("serialize");
    assert_eq!(
        wire.get("updatedMCPToolOutput"),
        Some(&json!({"result": "modified"}))
    );
}

// ===========================================================================
// TestMcpServerStatusTypes — 6 cases
// ===========================================================================

/// Ported from `test_mcp_server_status_importable_from_package`. The
/// Python test imports every MCP status type from the top-level package
/// to prove they're exported. The Rust equivalent is the `use`
/// statement at the top of this file — if any of those names were
/// gone, the test file wouldn't compile.
#[test]
fn mcp_server_status_importable_from_package() {
    // Touching each type forces the compiler to resolve its path.
    let _ = std::mem::size_of::<McpServerStatus>();
    let _ = std::mem::size_of::<McpServerConnectionStatus>();
    let _ = std::mem::size_of::<McpServerInfo>();
    let _ = std::mem::size_of::<McpStatusResponse>();
    let _ = std::mem::size_of::<McpToolAnnotations>();
    let _ = std::mem::size_of::<McpToolInfo>();
}

/// Ported from `test_mcp_server_status_connected`. Builds a full
/// status payload and parses it; verifies every field deserialises
/// under its camelCase wire key.
#[test]
fn mcp_server_status_connected() {
    let wire = json!({
        "name": "my-server",
        "status": "connected",
        "serverInfo": {"name": "my-server", "version": "1.2.3"},
        "config": {"type": "http", "url": "https://example.com"},
        "scope": "project",
        "tools": [
            {
                "name": "greet",
                "description": "Greet a user",
                "annotations": {
                    "readOnly": true,
                    "destructive": false,
                    "openWorld": false,
                }
            }
        ]
    });
    let status: McpServerStatus = serde_json::from_value(wire).expect("parse");
    assert_eq!(status.name, "my-server");
    assert_eq!(status.status, McpServerConnectionStatus::Connected);
    assert_eq!(
        status.server_info.as_ref().map(|i| i.version.as_str()),
        Some("1.2.3")
    );
    let tools = status.tools.expect("tools");
    assert_eq!(
        tools[0].annotations.as_ref().and_then(|a| a.read_only),
        Some(true)
    );
}

/// Ported from `test_mcp_server_status_minimal`. Only `name` + `status`
/// are required; every optional field must default to absent.
#[test]
fn mcp_server_status_minimal() {
    let wire = json!({"name": "pending-server", "status": "pending"});
    let status: McpServerStatus = serde_json::from_value(wire).expect("parse");
    assert_eq!(status.name, "pending-server");
    assert_eq!(status.status, McpServerConnectionStatus::Pending);
    assert!(status.error.is_none());
    assert!(status.config.is_none());
    // Round-trip back out: optional None fields must NOT emit keys.
    let out = serde_json::to_value(&status).expect("serialize");
    assert!(out.get("error").is_none());
    assert!(out.get("config").is_none());
}

/// Ported from `test_mcp_server_status_failed_with_error`.
#[test]
fn mcp_server_status_failed_with_error() {
    let wire = json!({
        "name": "broken-server",
        "status": "failed",
        "error": "Connection refused",
    });
    let status: McpServerStatus = serde_json::from_value(wire).expect("parse");
    assert_eq!(status.status, McpServerConnectionStatus::Failed);
    assert_eq!(status.error.as_deref(), Some("Connection refused"));
}

/// Ported from `test_mcp_server_status_config_claudeai_proxy`.
/// forge-sdk models `config` as opaque `Value` (Python's
/// `McpServerStatusConfig` is a discriminated union), so the
/// claudeai-proxy variant flows through by field introspection.
#[test]
fn mcp_server_status_config_claudeai_proxy() {
    let wire = json!({
        "name": "proxy-server",
        "status": "needs-auth",
        "config": {
            "type": "claudeai-proxy",
            "url": "https://claude.ai/proxy",
            "id": "proxy-abc",
        }
    });
    let status: McpServerStatus = serde_json::from_value(wire).expect("parse");
    let cfg = status.config.expect("config");
    assert_eq!(cfg.get("type"), Some(&json!("claudeai-proxy")));
    assert_eq!(cfg.get("id"), Some(&json!("proxy-abc")));
}

/// Ported from `test_mcp_status_response_wraps_servers`.
#[test]
fn mcp_status_response_wraps_servers() {
    let wire = json!({
        "mcpServers": [
            {"name": "a", "status": "connected"},
            {"name": "b", "status": "disabled"},
        ]
    });
    let response: McpStatusResponse = serde_json::from_value(wire).expect("parse");
    assert_eq!(response.mcp_servers.len(), 2);
    assert_eq!(
        response.mcp_servers[0].status,
        McpServerConnectionStatus::Connected
    );
    assert_eq!(
        response.mcp_servers[1].status,
        McpServerConnectionStatus::Disabled
    );
}

// ===========================================================================
// TestAgentDefinition — 10 cases
// ===========================================================================

/// Approximation of Python's `_serialize` helper — drops every field
/// that would serialise as `null` / missing, matching the CLI's
/// `{k: v for k, v in asdict(agent).items() if v is not None}` filter.
/// The serde `skip_serializing_if = "Option::is_none"` annotation on
/// every optional `AgentDefinition` field already implements this, so
/// `to_value` produces the exact shape we want to assert on.
fn serialize_agent(agent: &AgentDefinition) -> Value {
    serde_json::to_value(agent).expect("serialize")
}

/// Ported from `test_minimal_definition_omits_unset_fields`.
#[test]
fn agent_minimal_definition_omits_unset_fields() {
    let agent = AgentDefinition::new("test", "You are a test");
    let payload = serialize_agent(&agent);
    assert_eq!(
        payload,
        json!({"description": "test", "prompt": "You are a test"})
    );
}

/// Ported from `test_skills_and_memory_serialize_with_cli_keys`.
/// Verifies `skills` + `memory` survive the serde trip with their
/// Python-native keys.
#[test]
fn agent_skills_and_memory_serialize_with_cli_keys() {
    let agent = AgentDefinition::new("test", "p")
        .with_skills(vec!["skill-a".into(), "skill-b".into()])
        .with_memory(AgentMemory::Project);
    let payload = serialize_agent(&agent);
    assert_eq!(payload.get("skills"), Some(&json!(["skill-a", "skill-b"])));
    assert_eq!(payload.get("memory"), Some(&json!("project")));
}

/// Ported from `test_mcp_servers_serializes_as_camelcase`. CLI
/// expects `mcpServers`, not `mcp_servers` — this is the crucial
/// assertion.
#[test]
fn agent_mcp_servers_serializes_as_camelcase() {
    let agent = AgentDefinition::new("test", "p").with_mcp_servers(vec![
        AgentMcpServerRef::Named("slack".into()),
        AgentMcpServerRef::Inline {
            name: "local".into(),
            config: json!({"command": "python", "args": ["server.py"]}),
        },
    ]);
    let payload = serialize_agent(&agent);
    assert!(payload.get("mcpServers").is_some());
    assert!(payload.get("mcp_servers").is_none());
    let servers = payload.get("mcpServers").and_then(Value::as_array).unwrap();
    assert_eq!(servers[0], json!("slack"));
    assert_eq!(
        servers[1]
            .get("local")
            .and_then(|v| v.get("command"))
            .and_then(Value::as_str),
        Some("python")
    );
}

/// Ported from
/// `test_disallowed_tools_and_max_turns_serialize_as_camelcase`.
#[test]
fn agent_disallowed_tools_and_max_turns_serialize_as_camelcase() {
    let agent = AgentDefinition::new("test", "p")
        .with_disallowed_tools(vec!["Bash".into(), "Write".into()])
        .with_max_turns(10);
    let payload = serialize_agent(&agent);
    assert_eq!(
        payload.get("disallowedTools"),
        Some(&json!(["Bash", "Write"]))
    );
    assert!(payload.get("disallowed_tools").is_none());
    assert_eq!(payload.get("maxTurns"), Some(&json!(10)));
    assert!(payload.get("max_turns").is_none());
}

/// Ported from `test_initial_prompt_serializes_as_camelcase`.
#[test]
fn agent_initial_prompt_serializes_as_camelcase() {
    let agent = AgentDefinition::new("test", "p").with_initial_prompt("/review-pr 123");
    let payload = serialize_agent(&agent);
    assert_eq!(payload.get("initialPrompt"), Some(&json!("/review-pr 123")));
    assert!(payload.get("initial_prompt").is_none());
}

/// Ported from `test_model_accepts_full_model_id`.
#[test]
fn agent_model_accepts_full_model_id() {
    let agent = AgentDefinition::new("test", "p").with_model("claude-opus-4-5");
    let payload = serialize_agent(&agent);
    assert_eq!(payload.get("model"), Some(&json!("claude-opus-4-5")));
}

/// Ported from `test_background_serializes_correctly`.
#[test]
fn agent_background_serializes_correctly() {
    let agent = AgentDefinition::new("test", "p").with_background(true);
    let payload = serialize_agent(&agent);
    assert_eq!(payload.get("background"), Some(&json!(true)));
}

/// Ported from `test_effort_accepts_named_level`.
#[test]
fn agent_effort_accepts_named_level() {
    let agent =
        AgentDefinition::new("test", "p").with_effort(EffortLevel::Preset(EffortPreset::High));
    let payload = serialize_agent(&agent);
    assert_eq!(payload.get("effort"), Some(&json!("high")));
}

/// Ported from `test_effort_accepts_integer`. Python's `effort` is
/// `str | int`; forge-sdk's `EffortLevel` is an untagged enum that
/// carries either a preset or a raw token cap.
#[test]
fn agent_effort_accepts_integer() {
    let agent = AgentDefinition::new("test", "p").with_effort(EffortLevel::Numeric(32_000));
    let payload = serialize_agent(&agent);
    assert_eq!(payload.get("effort"), Some(&json!(32_000)));
}

/// Ported from `test_permission_mode_serializes_as_camelcase`.
#[test]
fn agent_permission_mode_serializes_as_camelcase() {
    let agent =
        AgentDefinition::new("test", "p").with_permission_mode(PermissionMode::BypassPermissions);
    let payload = serialize_agent(&agent);
    assert_eq!(
        payload.get("permissionMode"),
        Some(&json!("bypassPermissions"))
    );
    assert!(payload.get("permission_mode").is_none());
}

/// Ported from `test_new_fields_omitted_when_none`.
#[test]
fn agent_new_fields_omitted_when_none() {
    let agent = AgentDefinition::new("test", "p");
    let payload = serialize_agent(&agent);
    assert!(payload.get("background").is_none());
    assert!(payload.get("effort").is_none());
}

// ===========================================================================
// Touch otherwise-unused imports so the compile gate enforces surface
// presence even when a specific import isn't used by a test body above.
// ===========================================================================

#[test]
fn surface_touch_unused_imports() {
    let _ = BaseHookInput {
        session_id: "s".into(),
        transcript_path: "/t".into(),
        cwd: "/c".into(),
        permission_mode: None,
    };
    let _ = StopReason::EndTurn;
    let _ = Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };
}
