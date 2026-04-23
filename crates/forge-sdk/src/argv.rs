//! Pure-function argv composition. Takes an [`Options`] and produces
//! the `Vec<String>` to pass to the `claude` subprocess. Kept separate
//! from [`transport::process`](crate::transport::process) so the
//! transport layer doesn't import MCP orchestration types.

use crate::Error;
use crate::mcp::orchestration::McpHosts;
use crate::options::{
    Options, PermissionMode, SdkPluginConfig, SystemPromptKind, ThinkingConfig, ToolsPreset,
};

/// Build the subprocess argv from [`Options`], matching Python SDK's
/// `_build_command` byte-for-byte where possible. Exposed `pub` so
/// advanced callers can inspect the argv without spawning.
///
/// # Errors
///
/// [`Error::MessageParse`] when `options.sandbox` fails to serialise
/// (refuses to spawn un-sandboxed when the caller asked for sandboxing).
#[allow(clippy::too_many_lines)]
pub fn build_args(options: &Options) -> Result<Vec<String>, Error> {
    let mut args: Vec<String> = Vec::new();

    // Python SDK leads every invocation with these three flags
    // (`_internal/transport/subprocess_cli.py:207`).
    args.push("--output-format".into());
    args.push("stream-json".into());
    args.push("--verbose".into());

    // system_prompt — Python always emits one of four flag forms
    // (`subprocess_cli.py:209-218`), including an explicit
    // `--system-prompt ""` when the option is unset so the CLI
    // doesn't fall back to its builtin prompt. Match byte-for-byte.
    match options.system_prompt.as_ref() {
        None => {
            args.push("--system-prompt".into());
            args.push(String::new());
        }
        Some(SystemPromptKind::Inline(text)) => {
            args.push("--system-prompt".into());
            args.push(text.clone());
        }
        Some(SystemPromptKind::File(path)) => {
            args.push("--system-prompt-file".into());
            args.push(path.to_string_lossy().into_owned());
        }
        Some(SystemPromptKind::Preset { append, .. }) => {
            // `exclude_dynamic_sections` is delivered in the initialize
            // control_request body (client.rs), NOT on argv.
            if let Some(text) = append {
                args.push("--append-system-prompt".into());
                args.push(text.clone());
            }
        }
    }

    // tools (base set). Python emits `--tools default` for the preset,
    // `--tools <csv>` for a concrete list, `--tools ""` for an empty list.
    if let Some(tools) = &options.tools {
        match tools {
            ToolsPreset::Default => {
                args.push("--tools".into());
                args.push("default".into());
            }
            ToolsPreset::List(names) => {
                args.push("--tools".into());
                args.push(names.join(","));
            }
        }
    }

    // --allowedTools (camelCase per Python SDK). Combines explicit
    // allowed_tools + Skill injection.
    let mut allowed: Vec<String> = options.allowed_tools.clone();
    for skill in &options.skills {
        if skill == "all" {
            allowed.push("Skill".into());
        } else {
            allowed.push(format!("Skill({skill})"));
        }
    }
    if !allowed.is_empty() {
        args.push("--allowedTools".into());
        args.push(allowed.join(","));
    }

    if let Some(n) = options.max_turns {
        args.push("--max-turns".into());
        args.push(n.to_string());
    }
    if let Some(budget) = options.max_budget_usd {
        args.push("--max-budget-usd".into());
        args.push(budget.to_string());
    }
    if !options.disallowed_tools.is_empty() {
        args.push("--disallowedTools".into());
        args.push(options.disallowed_tools.join(","));
    }
    if let Some(tb) = options.task_budget {
        args.push("--task-budget".into());
        args.push(tb.to_string());
    }
    if let Some(model) = &options.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    if let Some(fb) = &options.fallback_model {
        args.push("--fallback-model".into());
        args.push(fb.clone());
    }
    if !options.betas.is_empty() {
        args.push("--betas".into());
        args.push(options.betas.join(","));
    }
    if let Some(name) = &options.permission_prompt_tool_name {
        args.push("--permission-prompt-tool".into());
        args.push(name.clone());
    }
    // Python SDK only emits `--permission-mode` when the caller set
    // one explicitly. We mirror that: the CLI default is already
    // `default`, so omitting the flag on the default variant avoids
    // argv drift and also lets the CLI honour any user-level override.
    if options.permission_mode != PermissionMode::Default {
        args.push("--permission-mode".into());
        args.push(options.permission_mode.as_cli_arg().into());
    }
    if options.continue_conversation {
        args.push("--continue".into());
    }
    if let Some(resume) = &options.resume {
        args.push("--resume".into());
        args.push(resume.clone());
    }
    if let Some(sid) = &options.session_id {
        args.push("--session-id".into());
        args.push(sid.clone());
    }
    // --settings (with optional sandbox merge). Python's
    // `_build_settings_value` — resolves settings + sandbox into one CLI
    // argument, either a file path or an inline JSON string.
    if let Some(value) = options.build_settings_value()? {
        args.push("--settings".into());
        args.push(value);
    }

    for dir in &options.add_dirs {
        args.push("--add-dir".into());
        args.push(dir.to_string_lossy().into_owned());
    }

    // MCP: pass --mcp-config '<inline-json>' when servers are registered.
    // Python SDK uses inline JSON (not a temp file) with {"type": "sdk"}
    // entries to signal in-process hosting; external servers carry their
    // own stdio / SSE / HTTP config verbatim.
    let hosts = McpHosts::new(
        options.mcp_servers.clone(),
        options.external_mcp_servers.clone(),
    );
    if !hosts.is_empty() {
        args.push("--mcp-config".into());
        args.push(hosts.config_argv());
    }

    if options.include_partial_messages {
        args.push("--include-partial-messages".into());
    }
    if options.fork_session {
        args.push("--fork-session".into());
    }
    // NB: enable_file_checkpointing is delivered via the
    // CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING env var, not a CLI flag
    // (Python subprocess_cli.py:436-437). Wired in transport/process.rs.

    // --setting-sources: explicit override wins; otherwise default to
    // user,project when skills is set (per Python SDK behaviour).
    let setting_sources: Option<Vec<String>> = options.setting_sources.clone().or_else(|| {
        if options.skills.is_empty() {
            None
        } else {
            Some(vec!["user".into(), "project".into()])
        }
    });
    if let Some(sources) = setting_sources {
        args.push(format!("--setting-sources={}", sources.join(",")));
    }

    for plugin in &options.plugins {
        match plugin {
            SdkPluginConfig::Local { path } => {
                args.push("--plugin-dir".into());
                args.push(path.to_string_lossy().into_owned());
            }
        }
    }

    // extra_args — arbitrary CLI flags. `None` value = bare flag.
    for (flag, maybe_val) in &options.extra_args {
        args.push(format!("--{flag}"));
        if let Some(v) = maybe_val {
            args.push(v.clone());
        }
    }

    // Resolve thinking config → --thinking / --max-thinking-tokens.
    // `thinking` takes precedence over the deprecated `max_thinking_tokens`.
    if let Some(t) = &options.thinking {
        match t {
            ThinkingConfig::Adaptive => {
                args.push("--thinking".into());
                args.push("adaptive".into());
            }
            ThinkingConfig::Enabled { budget_tokens } => {
                args.push("--max-thinking-tokens".into());
                args.push(budget_tokens.to_string());
            }
            ThinkingConfig::Disabled => {
                args.push("--thinking".into());
                args.push("disabled".into());
            }
        }
    } else if let Some(n) = options.max_thinking_tokens {
        args.push("--max-thinking-tokens".into());
        args.push(n.to_string());
    }

    if let Some(effort) = &options.effort {
        args.push("--effort".into());
        args.push(effort.as_cli_arg());
    }

    if let Some(schema) = options.output_format_json_schema() {
        args.push("--json-schema".into());
        args.push(schema);
    }

    // Always use streaming mode with stdin (matching TypeScript SDK).
    // This allows agents and other large configs to be sent via
    // `initialize` request rather than argv.
    args.push("--input-format".into());
    args.push("stream-json".into());

    Ok(args)
}
