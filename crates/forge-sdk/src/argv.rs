//! Pure-function argv composition. Takes an [`Options`] and produces
//! the `Vec<String>` to pass to the `claude` subprocess. Kept separate
//! from [`transport::process`](crate::transport::process) so the
//! transport layer doesn't import MCP orchestration types.

use crate::Error;
use crate::mcp::orchestration::McpHosts;
use crate::options::{Options, PermissionMode, SdkPluginConfig, SystemPromptKind};

/// Build the subprocess argv from [`Options`], SDK's
/// `_build_command` byte-for-byte where possible. Exposed `pub` so
/// advanced callers can inspect the argv without spawning.
///
/// # Errors
///
/// [`Error::MessageParse`] when `options.sandbox` fails to serialise
/// (refuses to spawn un-sandboxed when the caller asked for sandboxing).
pub fn build_args(options: &Options) -> Result<Vec<String>, Error> {
    let mut args: Vec<String> = Vec::new();

    // the CLI leads every invocation with these three flags
    //.
    args.push("--output-format".into());
    args.push("stream-json".into());
    args.push("--verbose".into());

    // system_prompt - the CLI always emits one of four flag forms,
    // including an explicit
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

    // --allowedTools (camelCase on the wire).
    if !options.allowed_tools.is_empty() {
        args.push("--allowedTools".into());
        args.push(options.allowed_tools.join(","));
    }

    if let Some(n) = options.max_turns {
        args.push("--max-turns".into());
        args.push(n.to_string());
    }
    if let Some(model) = &options.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    if let Some(name) = &options.permission_prompt_tool_name {
        args.push("--permission-prompt-tool".into());
        args.push(name.clone());
    }
    // the CLI only emits `--permission-mode` when the caller set
    // one explicitly. We mirror that: the CLI default is already
    // `default`, so omitting the flag on the default variant avoids
    // argv drift and also lets the CLI honour any user-level override.
    if options.permission_mode != PermissionMode::Ask {
        args.push("--permission-mode".into());
        args.push(options.permission_mode.as_cli_arg().into());
    }
    if let Some(resume) = &options.resume {
        args.push("--resume".into());
        args.push(resume.clone());
    }
    if let Some(sid) = &options.session_id {
        args.push("--session-id".into());
        args.push(sid.clone());
    }
    // --settings (with optional sandbox merge). Resolves settings +
    // sandbox into one CLI argument, either a file path or an inline
    // JSON string.
    if let Some(value) = options.build_settings_value()? {
        args.push("--settings".into());
        args.push(value);
    }

    // MCP: pass --mcp-config '<inline-json>' when servers are registered.
    // the CLI uses inline JSON (not a temp file) with {"type": "sdk"}
    // entries to signal in-process hosting; external servers carry their
    // own stdio / SSE / HTTP config verbatim.
    let hosts = McpHosts::new(options.mcp_servers.clone(), options.external_mcp_servers.clone());
    if !hosts.is_empty() {
        args.push("--mcp-config".into());
        args.push(hosts.config_argv());
    }

    if let Some(sources) = &options.setting_sources {
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

    // extra_args - arbitrary CLI flags. `None` value = bare flag.
    for (flag, maybe_val) in &options.extra_args {
        args.push(format!("--{flag}"));
        if let Some(v) = maybe_val {
            args.push(v.clone());
        }
    }

    if let Some(effort) = &options.effort {
        args.push("--effort".into());
        args.push(effort.as_cli_arg());
    }

    // Always use streaming mode with stdin (matching TypeScript SDK).
    // This allows agents and other large configs to be sent via
    // `initialize` request rather than argv.
    args.push("--input-format".into());
    args.push("stream-json".into());

    Ok(args)
}
