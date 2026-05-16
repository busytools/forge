pub mod agent;
pub mod app;
pub mod error;
pub mod logging;
pub mod perf;
pub mod ui;

use clap::{Parser, ValueEnum};

/// Full version string for the welcome banner + status panel.
///
/// Always carries the short SHA so a screenshot is enough to
/// identify the running build. On `main` (and detached HEAD) the
/// stamp adds ` · <sha>`; on any other branch the stamp adds
/// ` · <sha> (<branch>)`.
pub const FORGE_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), env!("FORGE_BUILD_SUFFIX_FULL"));

/// Short version string for tight slots (Projects pane bottom row,
/// launchpad version line). Always carries the short SHA as
/// `+<sha>` so the running build is identifiable from a screenshot.
pub const FORGE_VERSION_SHORT: &str =
    concat!(env!("CARGO_PKG_VERSION"), env!("FORGE_BUILD_SUFFIX_SHORT"));

#[derive(Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum DiagnosticsPreset {
    Runtime,
    Session,
    Render,
    Bridge,
    Full,
}

impl DiagnosticsPreset {
    pub fn filter_directives(&self) -> &'static str {
        match self {
            Self::Runtime => {
                "info,bridge.lifecycle=debug,bridge.protocol=debug,app.session=debug,app.tool=debug,app.command=debug,app.permission=debug,app.network=debug,app.update=debug"
            }
            Self::Session => {
                "info,bridge.lifecycle=debug,bridge.protocol=debug,app.session=debug,app.permission=debug,app.command=debug"
            }
            Self::Render => {
                "info,app.render=trace,app.cache=debug,app.input=debug,app.paste=debug,app.perf=info"
            }
            Self::Bridge => {
                "info,bridge.lifecycle=debug,bridge.protocol=debug,bridge.sdk=debug,bridge.permission=debug,bridge.mcp=debug"
            }
            Self::Full => {
                "info,app.render=trace,app.perf=info,bridge.lifecycle=debug,bridge.protocol=debug,bridge.sdk=debug,bridge.permission=debug,bridge.mcp=debug,app.session=debug,app.tool=debug,app.command=debug,app.permission=debug,app.network=debug,app.update=debug,app.cache=debug,app.input=debug,app.paste=debug,app.config=debug,app.auth=debug"
            }
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "forge", about = "Native Rust terminal for Claude Code")]
#[command(
    after_help = "Examples:\n  forge --enable-logs --diagnostics-preset session\n  forge --enable-logs --diagnostics-preset render\n  forge --features perf --enable-logs --enable-perf --diagnostics-preset full"
)]
// Each bool maps 1:1 to a CLI flag — clap-derive needs them as struct fields.
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Project name to open. When omitted, opens the project marked
    /// `default = true` in forge.toml. Must match a project's `name`
    /// field in forge.toml exactly.
    #[arg(value_name = "PROJECT")]
    pub project: Option<String>,

    /// Generate a shell completion script and print to stdout, then
    /// exit. Hidden from --help; called by install-forge.sh.
    #[arg(long, value_name = "SHELL", hide = true)]
    pub generate_completion: Option<clap_complete::Shell>,

    /// Enable runtime diagnostics using a default log path when `--log-file` is omitted.
    #[arg(long)]
    pub enable_logs: bool,

    /// Named diagnostics preset for common logging workflows.
    /// Ignored when `--log-filter` is provided explicitly.
    #[arg(long, value_enum)]
    pub diagnostics_preset: Option<DiagnosticsPreset>,

    /// Write tracing diagnostics to a file.
    ///
    /// When omitted but logging is otherwise enabled via `--enable-logs`,
    /// `--diagnostics-preset`, `--log-filter`, or `RUST_LOG`, a
    /// default log path is used.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<std::path::PathBuf>,

    /// Tracing filter directives (example: `info,app.render=trace`).
    /// Overrides `--diagnostics-preset` and falls back to `RUST_LOG` when omitted.
    #[arg(long, value_name = "FILTER")]
    pub log_filter: Option<String>,

    /// Enable perf telemetry using a default sidecar path when `--perf-log` is omitted.
    /// Requires a binary built with `--features perf`.
    #[arg(long)]
    pub enable_perf: bool,

    /// Write high-frequency perf telemetry to a sidecar JSON file (requires `--features perf` build).
    #[arg(long, value_name = "PATH")]
    pub perf_log: Option<std::path::PathBuf>,

    /// Append to `--perf-log` instead of truncating on startup.
    #[arg(long)]
    pub perf_append: bool,
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_parses_bare_forge_with_default_log_and_perf_flags() {
        let cli = Cli::try_parse_from(["forge"]).expect("parse");
        assert!(cli.project.is_none());
        assert!(cli.generate_completion.is_none());
        assert!(!cli.enable_logs);
        assert!(cli.diagnostics_preset.is_none());
        assert!(cli.log_file.is_none());
        assert!(cli.log_filter.is_none());
        assert!(!cli.enable_perf);
        assert!(cli.perf_log.is_none());
        assert!(!cli.perf_append);
    }

    #[test]
    fn cli_rejects_legacy_resume_flag() {
        assert!(Cli::try_parse_from(["forge", "--resume", "abc-123"]).is_err());
    }

    #[test]
    fn cli_accepts_optional_positional_project() {
        let cli = Cli::try_parse_from(["forge", "dotfiles"]).expect("parse");
        assert_eq!(cli.project.as_deref(), Some("dotfiles"));
    }

    #[test]
    fn cli_generate_completion_zsh_parses() {
        let cli = Cli::try_parse_from(["forge", "--generate-completion", "zsh"]).expect("parse");
        assert!(matches!(cli.generate_completion, Some(clap_complete::Shell::Zsh)));
    }

    #[test]
    fn cli_help_does_not_surface_generate_completion() {
        // Render --help and confirm the hidden flag isn't there.
        // (Smoke test — clap's `hide = true` is the source of truth.)
        let mut cmd = Cli::command();
        let mut out = Vec::new();
        cmd.write_help(&mut out).expect("write_help");
        let help_text = String::from_utf8(out).expect("utf8");
        assert!(
            !help_text.contains("generate-completion"),
            "hidden flag should not appear in --help"
        );
    }
}
