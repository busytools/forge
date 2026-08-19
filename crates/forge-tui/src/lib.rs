pub mod agent;
pub mod app;
pub mod error;
pub mod logging;
pub mod perf;
pub mod startup;
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
#[command(name = "forge", about = "Native Rust terminal for Claude Code", version)]
pub struct Cli {
    /// Project to open, matching an `[[orgs.projects]]` `name` in
    /// forge.toml exactly. When omitted, every project with
    /// `auto_start = true` spawns and the first one declared takes
    /// focus; with no auto_start project, the alphabetically-first
    /// project opens.
    #[arg(value_name = "PROJECT")]
    pub project: Option<String>,

    /// Start every boot session fresh: auto_start projects and their
    /// workers spawn as new sessions instead of resuming. Only affects
    /// the startup wave - clicking a sleeping project later still resumes.
    #[arg(long)]
    pub new: bool,

    /// Generate a shell completion script and print to stdout, then
    /// exit. Hidden from --help; called by `scripts/install.sh`.
    #[arg(long, value_name = "SHELL", hide = true)]
    pub generate_completion: Option<clap_complete::Shell>,

    /// Named diagnostics preset for common logging workflows.
    /// Ignored when `--log-filter` is provided explicitly.
    #[arg(long, value_enum)]
    pub diagnostics_preset: Option<DiagnosticsPreset>,

    /// Write tracing diagnostics to a file.
    ///
    /// When omitted but logging is otherwise enabled via
    /// `--diagnostics-preset`, `--log-filter`, or `RUST_LOG`, a
    /// default log path is used.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<std::path::PathBuf>,

    /// Tracing filter directives (example: `info,app.render=trace`).
    /// Overrides `--diagnostics-preset` and falls back to `RUST_LOG` when omitted.
    #[arg(long, value_name = "FILTER")]
    pub log_filter: Option<String>,

    /// Write high-frequency perf telemetry to a sidecar JSON file (requires `--features perf` build).
    #[arg(long, value_name = "PATH")]
    pub perf_log: Option<std::path::PathBuf>,
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
        assert!(cli.diagnostics_preset.is_none());
        assert!(cli.log_file.is_none());
        assert!(cli.log_filter.is_none());
        assert!(cli.perf_log.is_none());
    }

    #[test]
    fn cli_rejects_legacy_resume_flag() {
        assert!(Cli::try_parse_from(["forge", "--resume", "abc-123"]).is_err());
    }

    #[test]
    fn new_flag_parses() {
        let cli = Cli::try_parse_from(["forge", "--new"]).expect("parse");
        assert!(cli.new);
    }

    #[test]
    fn new_flag_defaults_false() {
        let cli = Cli::try_parse_from(["forge"]).expect("parse");
        assert!(!cli.new);
    }

    #[test]
    fn version_flag_exits_with_display_version() {
        // clap handles --version as print-and-exit inside parse(), so
        // it surfaces as an Err with kind DisplayVersion (same shape as
        // --help / bad args) and never returns a parsed Cli.
        let err = Cli::try_parse_from(["forge", "--version"]).expect_err("--version exits");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
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
        // (Smoke test - clap's `hide = true` is the source of truth.)
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
