//! Wire-shape mirror of [`forge_sdk::Options`] + the
//! `parse_spawn_params` deserialiser.
//!
//! Extracted from `methods/session.rs` so the actor body and handler
//! proxies don't have to scroll past 130+ lines of wire-struct
//! plumbing. The submodule is private; the only thing exposed back to
//! `methods::session` is `parse_spawn_params`, which the dispatch
//! arm + the `forged-conformance` harness call.

use forge_sdk::agents::{EffortLevel, EffortPreset};
use forge_sdk::{OptionsBuilder, SdkPluginConfig, SystemPromptKind, ThinkingConfig};
use serde_json::Value;

use crate::Error;
use crate::sdk_callbacks::WireHookSpec;

use super::SpawnParams;

/// Wire-shape mirror of [`forge_sdk::Options`]. Lifted from the public
/// SDK surface and decoupled from it: when the SDK adds a field we add
/// it here too; when the SDK drops a field we drop it here and document
/// the back-compat in the changelog.
///
/// **Fields without wire representation:** `can_use_tool`,
/// `hooks_callback`, in-process `mcp_servers`, custom stderr handlers,
/// and any other field whose value is a Rust function, trait object, or
/// in-process handle. Hooks fan out over reverse-RPC instead — see
/// [`WireHookSpec`].
///
/// `deny_unknown_fields` is set so typos in supported field names
/// surface as errors rather than being silently dropped — but the same
/// validation is what produces the "unknown field" error a client sees
/// when they pass `can_use_tool`. The wire-spec doc and SDK API ref
/// must document the supported subset to keep the experience clear; a
/// future enhancement could capture the unsupported names and emit a
/// targeted "this field has no wire representation" error instead of
/// the generic "unknown field" message, but the cost/benefit of a
/// custom Deserialize impl outweighs the ergonomic win for now.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Options field-for-field; mirroring a foreign struct's bool flags is intentional"
)]
struct WireOptions {
    binary: Option<String>,
    cwd: Option<String>,
    resume: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    allowed_tools: Vec<String>,
    skills: Vec<String>,
    setting_sources: Option<Vec<String>>,
    exclude_dynamic_sections: Option<bool>,
    permission_prompt_tool_name: Option<String>,
    minimum_cli_version: Option<String>,
    projects_dir: Option<String>,
    system_prompt: Option<WireSystemPrompt>,
    tools: Option<WireTools>,
    disallowed_tools: Vec<String>,
    max_turns: Option<u64>,
    max_budget_usd: Option<f64>,
    fallback_model: Option<String>,
    betas: Vec<String>,
    continue_conversation: bool,
    session_id: Option<String>,
    include_partial_messages: bool,
    fork_session: bool,
    add_dirs: Vec<String>,
    plugins: Vec<WirePlugin>,
    env: std::collections::HashMap<String, String>,
    user: Option<String>,
    extra_args: std::collections::HashMap<String, Option<String>>,
    effort: Option<WireEffort>,
    thinking: Option<WireThinking>,
    max_thinking_tokens: Option<u64>,
    task_budget: Option<u64>,
    output_format: Option<serde_json::Value>,
    max_buffer_size: Option<usize>,
    enable_file_checkpointing: bool,
    settings: Option<String>,
    /// Hook registrations (M4). Each entry attaches a
    /// [`ForgedHookBridge`](crate::sdk_callbacks::ForgedHookBridge) for
    /// the given hook kind so the CLI's hook callbacks fan out over
    /// reverse-RPC.
    hooks: Vec<WireHookSpec>,
}

/// System-prompt wire shape. Mirrors [`forge_sdk::SystemPromptKind`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireSystemPrompt {
    /// `--system-prompt <text>`
    Inline {
        /// The literal prompt text.
        text: String,
    },
    /// `--system-prompt-file <path>`
    File {
        /// Path to the prompt file.
        path: String,
    },
    /// Preset (`claude_code`) with optional append text.
    Preset {
        /// Optional append payload — `--append-system-prompt <text>`.
        #[serde(default)]
        append: Option<String>,
    },
}

/// Plugin wire shape. Mirrors [`forge_sdk::SdkPluginConfig`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WirePlugin {
    /// Local filesystem plugin.
    Local {
        /// Directory containing the plugin.
        path: String,
    },
}

/// Tools-preset wire shape. Mirrors [`forge_sdk::ToolsPreset`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireTools {
    /// `--tools default`
    Default,
    /// Explicit `--tools <csv>` list.
    List {
        /// The tool list to forward.
        tools: Vec<String>,
    },
}

/// Thinking-config wire shape. Mirrors [`forge_sdk::ThinkingConfig`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireThinking {
    /// CLI picks per-turn.
    Adaptive,
    /// Thinking on with a per-turn token cap.
    Enabled {
        /// Per-turn budget.
        budget_tokens: u64,
    },
    /// Thinking off.
    Disabled,
}

/// Effort wire shape — string preset or numeric override.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum WireEffort {
    /// `low | medium | high | max`
    Preset(String),
    /// Numeric override.
    Numeric(i64),
}

/// Parse the full `session.spawn` params into a configured
/// [`forge_sdk::Options`] plus the [`WireHookSpec`] list. Replaces the
/// M2 stub.
///
/// Hooks are returned separately rather than baked into Options
/// because their attachment depends on the session id (each
/// [`ForgedHookBridge`](crate::sdk_callbacks::ForgedHookBridge) carries
/// the session id as a field), and the session id is minted by `spawn`
/// itself.
///
/// # Errors
///
/// [`Error::InvalidParams`] when the `options` blob fails serde
/// deserialisation, references an unknown enum variant (e.g. an
/// unrecognised `permission_mode`), or carries an unknown field.
#[allow(
    clippy::too_many_lines,
    reason = "one builder call per Options field by design; collapsing would obscure the wire-shape mapping"
)]
pub fn parse_spawn_params(raw: &Value) -> Result<SpawnParams, Error> {
    let opts_v = raw
        .get("options")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let wire: WireOptions = serde_json::from_value(opts_v)
        .map_err(|e| Error::InvalidParams(format!("options: {e}")))?;
    let hook_specs = wire.hooks.clone();

    let mut b = OptionsBuilder::new();
    if let Some(bin) = wire.binary {
        b = b.binary(bin);
    }
    if let Some(cwd) = wire.cwd {
        b = b.cwd(cwd);
    }
    if let Some(model) = wire.model {
        b = b.model(model);
    }
    if let Some(resume) = wire.resume {
        b = b.resume(resume);
    }
    if let Some(mode_str) = wire.permission_mode.as_deref() {
        b = b.permission_mode(crate::session_state::parse_permission_mode(mode_str)?);
    }
    if !wire.allowed_tools.is_empty() {
        b = b.allowed_tools(wire.allowed_tools);
    }
    if !wire.disallowed_tools.is_empty() {
        b = b.disallowed_tools(wire.disallowed_tools);
    }
    if !wire.skills.is_empty() {
        b = b.skills(wire.skills);
    }
    if let Some(sources) = wire.setting_sources {
        b = b.setting_sources(sources);
    }
    if let Some(v) = wire.exclude_dynamic_sections {
        b = b.exclude_dynamic_sections(v);
    }
    if let Some(name) = wire.permission_prompt_tool_name {
        b = b.permission_prompt_tool_name(name);
    }
    if let Some(min) = wire.minimum_cli_version {
        b = b.minimum_cli_version(Some(min));
    }
    if let Some(d) = wire.projects_dir {
        b = b.projects_dir(d);
    }
    if let Some(sp) = wire.system_prompt {
        let kind = match sp {
            WireSystemPrompt::Inline { text } => SystemPromptKind::Inline(text),
            WireSystemPrompt::File { path } => SystemPromptKind::File(path.into()),
            WireSystemPrompt::Preset { append } => SystemPromptKind::Preset {
                append,
                exclude_dynamic_sections: None,
            },
        };
        b = b.system_prompt(kind);
    }
    if let Some(t) = wire.tools {
        let preset = match t {
            WireTools::Default => forge_sdk::ToolsPreset::Default,
            WireTools::List { tools } => forge_sdk::ToolsPreset::List(tools),
        };
        b = b.tools(preset);
    }
    if let Some(n) = wire.max_turns {
        b = b.max_turns(n);
    }
    if let Some(n) = wire.max_budget_usd {
        b = b.max_budget_usd(n);
    }
    if let Some(m) = wire.fallback_model {
        b = b.fallback_model(m);
    }
    if !wire.betas.is_empty() {
        b = b.betas(wire.betas);
    }
    if wire.continue_conversation {
        b = b.continue_conversation(true);
    }
    if let Some(sid) = wire.session_id {
        b = b.session_id(sid);
    }
    if wire.include_partial_messages {
        b = b.include_partial_messages(true);
    }
    if wire.fork_session {
        b = b.fork_session(true);
    }
    if !wire.add_dirs.is_empty() {
        b = b.add_dirs(wire.add_dirs.into_iter().map(Into::into).collect());
    }
    if !wire.plugins.is_empty() {
        let plugins: Vec<SdkPluginConfig> = wire
            .plugins
            .into_iter()
            .map(|p| match p {
                WirePlugin::Local { path } => SdkPluginConfig::Local { path: path.into() },
            })
            .collect();
        b = b.plugins(plugins);
    }
    if !wire.env.is_empty() {
        b = b.envs(wire.env);
    }
    if let Some(u) = wire.user {
        b = b.user(u);
    }
    for (k, v) in wire.extra_args {
        b = b.extra_arg(k, v);
    }
    if let Some(eff) = wire.effort {
        let level = match eff {
            WireEffort::Preset(s) => match s.as_str() {
                "low" => EffortLevel::Preset(EffortPreset::Low),
                "medium" => EffortLevel::Preset(EffortPreset::Medium),
                "high" => EffortLevel::Preset(EffortPreset::High),
                "max" => EffortLevel::Preset(EffortPreset::Max),
                other => {
                    return Err(Error::InvalidParams(format!(
                        "effort: unknown preset '{other}'"
                    )));
                }
            },
            WireEffort::Numeric(n) => EffortLevel::Numeric(n),
        };
        b = b.effort(level);
    }
    if let Some(t) = wire.thinking {
        let cfg = match t {
            WireThinking::Adaptive => ThinkingConfig::Adaptive,
            WireThinking::Enabled { budget_tokens } => ThinkingConfig::Enabled { budget_tokens },
            WireThinking::Disabled => ThinkingConfig::Disabled,
        };
        b = b.thinking(cfg);
    }
    if let Some(t) = wire.max_thinking_tokens {
        b = b.max_thinking_tokens(t);
    }
    if let Some(t) = wire.task_budget {
        b = b.task_budget(t);
    }
    if let Some(v) = wire.output_format {
        b = b.output_format(v);
    }
    if let Some(n) = wire.max_buffer_size {
        b = b.max_buffer_size(n);
    }
    if wire.enable_file_checkpointing {
        b = b.enable_file_checkpointing(true);
    }
    if let Some(s) = wire.settings {
        b = b.settings(s);
    }

    Ok(SpawnParams {
        options: b.build(),
        hooks: hook_specs,
    })
}
