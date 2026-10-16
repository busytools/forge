use crate::cli::{Cli, DiagnosticsPreset};
use anyhow::Context as _;
use std::fs::{File, OpenOptions, create_dir_all, metadata, remove_file, rename};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;

pub mod targets {
    pub const APP_INPUT: &str = "app.input";
    pub const APP_LIFECYCLE: &str = "app.lifecycle";
    pub const APP_PERMISSION: &str = "app.permission";
    pub const BRIDGE_LIFECYCLE: &str = "bridge.lifecycle";
    pub const BRIDGE_MCP: &str = "bridge.mcp";
    pub const BRIDGE_PERMISSION: &str = "bridge.permission";
    pub const BRIDGE_PROTOCOL: &str = "bridge.protocol";
    pub const BRIDGE_SDK: &str = "bridge.sdk";
}

const DEFAULT_LOG_DIR: &str = "forge";
const DEFAULT_LOG_FILE_NAME: &str = "forge.log";
pub(crate) const LOG_ROTATION_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub(crate) const LOG_ROTATION_MAX_FILES: usize = 5;

pub struct LoggingRuntime {
    _guard: Option<WorkerGuard>,
}

impl LoggingRuntime {
    pub fn init(cli: &Cli) -> anyhow::Result<Self> {
        let Some(log_path) = resolve_log_path(cli)? else {
            return Ok(Self { _guard: None });
        };

        let directives = build_filter_directives(cli);
        let filter = tracing_subscriber::EnvFilter::try_new(directives.as_str())
            .map_err(|e| anyhow::anyhow!("invalid tracing filter `{directives}`: {e}"))?;
        // Always append so a restart immediately after a bug
        // doesn't overwrite the events that produced it. The window
        // keeps the active file plus `.1` through `.5`, so rotation
        // is capped at 10 MB × 6 slots (~60 MB total).
        let writer = RollingFileWriter::new(
            &log_path.path,
            true,
            LOG_ROTATION_MAX_BYTES,
            LOG_ROTATION_MAX_FILES,
        )?;
        let (non_blocking, guard) = tracing_appender::non_blocking(writer);

        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_env_filter(filter)
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_file(true)
            .with_line_number(true)
            .with_target(true)
            .try_init()
            .map_err(|e| anyhow::anyhow!("failed to initialize tracing subscriber: {e}"))?;

        tracing::info!(
            target: targets::APP_LIFECYCLE,
            event_name = "logging_initialized",
            message = "tracing subscriber initialized",
            log_file = %log_path.path.display(),
            log_path_source = log_path.source.as_str(),
            log_filter = %directives,
            log_rotation_max_bytes = LOG_ROTATION_MAX_BYTES,
            log_rotation_max_files = LOG_ROTATION_MAX_FILES,
            version = crate::FORGE_VERSION,
        );

        Ok(Self { _guard: Some(guard) })
    }
}

/// Default tracing filter. `info` baseline keeps signal-to-noise
/// sensible; the targets at `debug` are the ones we actually stare at
/// when triaging UI/event drops (input submission, the bridge connect
/// path). `agent.env_git` is there for a second reason: a git probe
/// missing `refs/remotes/origin/HEAD` is real and is not forge's
/// problem. At `debug` it stays readable without raising a warning
/// that says forge is unwell. `forge_workspace::output_tail` is there for
/// the same reason: a line in a command's output file that cannot be
/// decoded is the watched command's own output rather than a problem with
/// forge. (`forge_server` too: its session reads are one session's own
/// work.)
///
/// `forge_dictate` and `forge_workspace::dictate` are there for the
/// same reason again: a take that reaches its capture cap, a normalizer
/// that runs out of token budget and a take whose recognition failed are
/// one session's own work, not forge's health. They were demoted out of
/// `WARN` for it, so they are named here to stay readable.
///
/// The two `llama` targets are the dictation engine's own log bridge,
/// which reaches tracing through the llama-cpp-2 crate under both its
/// hyphenated target and its module path. What it reports is the model
/// loading itself, not forge, so it is not forge's warning to raise.
const DEFAULT_LOG_DIRECTIVES: &str = "info,\
    app.input=debug,\
    app.lifecycle=debug,\
    bridge.lifecycle=debug,\
    agent.env_git=debug,\
    forge_server=debug,\
    forge_agent::userdata::catalog::scan=debug,\
    forge_workspace::output_tail=debug,\
    forge_workspace::work=debug,\
    forge_workspace::browser=debug,\
    forge_workspace::dictate=debug,\
    forge_dictate=debug,\
    llama_cpp_2=error,\
    llama-cpp-2=error";

/// The preset's directives with the default set composed in, so a preset
/// extends the always-on coverage rather than replacing it: a target that
/// only the defaults name stays readable under every preset.
fn preset_extending_defaults(preset: &DiagnosticsPreset) -> String {
    compose_filter_directives(preset.filter_directives(), DEFAULT_LOG_DIRECTIVES)
}

/// No target can be empty, so this key collides with no real slot: every
/// bare level token shares it, because the parser resolves them all to the
/// filter's one global directive and the level is not part of that slot's
/// identity.
const GLOBAL_SLOT_KEY: &str = "";

/// A directive's slot in the filter, the part two directives naming the
/// same slot collide on. A bare level token (`info`, `4`, a case variant)
/// is the global directive, which every bare level shares. Anything else
/// keys by its target, cut at the `=` the parser would use: the first one
/// outside a brace shape.
fn directive_key(directive: &str) -> &str {
    if LevelFilter::from_str(directive).is_ok() {
        return GLOBAL_SLOT_KEY;
    }
    let mut depth = 0usize;
    for (index, ch) in directive.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '=' if depth == 0 => return &directive[..index],
            _ => {}
        }
    }
    directive
}

/// The two directive lists composed into one, a directive both name kept
/// once - the preset's copy, whatever level it sets - so a default never
/// overrides a slot the preset names. The preset's own order leads; the
/// defaults' directives it did not name follow.
fn compose_filter_directives(preset_directives: &str, default_directives: &str) -> String {
    let mut directives: Vec<&str> = Vec::new();
    for directive in preset_directives.split(',').chain(default_directives.split(',')) {
        let key = directive_key(directive);
        if !directives.iter().any(|kept| directive_key(kept) == key) {
            directives.push(directive);
        }
    }
    directives.join(",")
}

fn build_filter_directives(cli: &Cli) -> String {
    cli.log_filter
        .clone()
        .or_else(|| cli.diagnostics_preset.as_ref().map(preset_extending_defaults))
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| DEFAULT_LOG_DIRECTIVES.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogPathSource {
    Explicit,
    Default,
}

impl LogPathSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedLogPath {
    path: PathBuf,
    source: LogPathSource,
}

fn resolve_log_path(cli: &Cli) -> anyhow::Result<Option<ResolvedLogPath>> {
    if let Some(path) = cli.log_file.clone() {
        return Ok(Some(ResolvedLogPath { path, source: LogPathSource::Explicit }));
    }
    // Logging on by default: the rolling writer caps disk at ~60MB
    // (6 x 10MB), which is cheap enough that diagnostics need no opt-in.
    // `RUST_LOG=off` silences it.
    let path = default_log_path()?;
    Ok(Some(ResolvedLogPath { path, source: LogPathSource::Default }))
}

fn default_log_path() -> anyhow::Result<PathBuf> {
    let base_dir = default_diagnostics_dir()?;
    Ok(base_dir.join(DEFAULT_LOG_FILE_NAME))
}

fn default_diagnostics_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = dirs::data_local_dir() {
        return Ok(dir.join(DEFAULT_LOG_DIR).join("logs"));
    }
    if let Some(dir) = dirs::cache_dir() {
        return Ok(dir.join(DEFAULT_LOG_DIR).join("logs"));
    }
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(format!(".{DEFAULT_LOG_DIR}")).join("logs"));
    }
    // No state / cache / home dir available - refuse to fall back
    // to `std::env::current_dir()`. Where the binary was launched
    // must not change the log location: a `forge` invocation from
    // `~/Projects/forge` and one from `/tmp` must write to the same
    // path or to no path. Returning an error here disables logging
    // for the rare environment that has no resolvable user dirs.
    anyhow::bail!("no resolvable data/cache/home dir for diagnostics path")
}

/// Rotation is decided from the current `write` call's length, so a
/// caller must hand over one whole record per call and keep it under
/// the buffer capacity - a partial write re-enters the check with only
/// the remainder.
#[derive(Debug)]
pub struct RollingFileWriter {
    base_path: PathBuf,
    max_bytes: u64,
    max_files: usize,
    file: BufWriter<File>,
    current_size: u64,
}

impl RollingFileWriter {
    pub fn new(
        path: &Path,
        append: bool,
        max_bytes: u64,
        max_files: usize,
    ) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent)
                .with_context(|| format!("failed to create log directory {}", parent.display()))?;
        }
        if append {
            let current_size = metadata(path).map_or(0, |m| m.len());
            if current_size >= max_bytes {
                // The recursion's `clear_rotated_files` is the intended
                // outcome: an already-over-cap log is discarded, not
                // preserved as `.1`.
                rotate_file_window(path, max_files)?;
                return Self::new(path, false, max_bytes, max_files);
            }
            let file = open_log_file(path, true)?;
            return Ok(Self {
                base_path: path.to_path_buf(),
                max_bytes,
                max_files,
                file: BufWriter::new(file),
                current_size,
            });
        }

        clear_rotated_files(path, max_files)?;
        let file = open_log_file(path, false)?;
        Ok(Self {
            base_path: path.to_path_buf(),
            max_bytes,
            max_files,
            file: BufWriter::new(file),
            current_size: 0,
        })
    }

    fn rotate_if_needed(&mut self, incoming_len: usize) -> std::io::Result<()> {
        let incoming = u64::try_from(incoming_len).unwrap_or(u64::MAX);
        if self.current_size == 0 || self.current_size.saturating_add(incoming) <= self.max_bytes {
            return Ok(());
        }
        self.file.flush()?;
        rotate_file_window(&self.base_path, self.max_files)?;
        self.file = BufWriter::new(open_log_file(&self.base_path, false)?);
        self.current_size = 0;
        Ok(())
    }
}

impl Write for RollingFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.rotate_if_needed(buf.len())?;
        let written = self.file.write(buf)?;
        self.current_size =
            self.current_size.saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn open_log_file(path: &Path, append: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options.open(path)
}

fn rotate_file_window(base_path: &Path, max_files: usize) -> std::io::Result<()> {
    if max_files == 0 {
        if base_path.exists() {
            remove_file(base_path)?;
        }
        return Ok(());
    }

    let oldest = rotated_log_path(base_path, max_files);
    if oldest.exists() {
        remove_file(&oldest)?;
    }

    for index in (1..max_files).rev() {
        let from = rotated_log_path(base_path, index);
        if from.exists() {
            let to = rotated_log_path(base_path, index + 1);
            if to.exists() {
                remove_file(&to)?;
            }
            rename(&from, &to)?;
        }
    }

    if base_path.exists() {
        let first = rotated_log_path(base_path, 1);
        if first.exists() {
            remove_file(&first)?;
        }
        rename(base_path, first)?;
    }

    Ok(())
}

fn clear_rotated_files(base_path: &Path, max_files: usize) -> std::io::Result<()> {
    for index in 1..=max_files {
        let rotated = rotated_log_path(base_path, index);
        if rotated.exists() {
            remove_file(rotated)?;
        }
    }
    Ok(())
}

fn rotated_log_path(base_path: &Path, index: usize) -> PathBuf {
    let suffix = format!(".{index}");
    if let Some(name) = base_path.file_name().and_then(|name| name.to_str()) {
        base_path.with_file_name(format!("{name}{suffix}"))
    } else {
        let mut path = base_path.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_LOG_DIRECTIVES, RollingFileWriter, build_filter_directives, clear_rotated_files,
        compose_filter_directives, directive_key, resolve_log_path, rotated_log_path,
    };
    use crate::cli::{Cli, DiagnosticsPreset};
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn resolve_log_path_uses_explicit_path_when_provided() {
        let cli = Cli {
            project: None,
            new: false,
            generate_completion: None,
            diagnostics_preset: None,
            log_file: Some(PathBuf::from("custom.log")),
            log_filter: None,
        };

        let resolved = resolve_log_path(&cli).expect("resolve succeeds").expect("path exists");
        assert_eq!(resolved.path, PathBuf::from("custom.log"));
        assert_eq!(resolved.source.as_str(), "explicit");
    }

    #[test]
    fn resolve_log_path_returns_default_with_no_triggers_set() {
        // Logs are on by default - no CLI flag, env var, or filter
        // needed. Pins the policy so a future refactor can't
        // accidentally re-introduce the "opt-in" gate.
        let cli = Cli {
            project: None,
            new: false,
            generate_completion: None,
            diagnostics_preset: None,
            log_file: None,
            log_filter: None,
        };

        let resolved = resolve_log_path(&cli).expect("resolve succeeds").expect("path exists");
        assert_eq!(resolved.source.as_str(), "default");
        let path = resolved.path.to_string_lossy().replace('\\', "/");
        assert!(path.ends_with("forge/logs/forge.log"));
    }

    #[test]
    fn default_log_directives_target_the_drop_diagnosis_paths() {
        // `app.input` covers input submission; `bridge.lifecycle`
        // covers connect/spawn. Anything dropping out of these
        // regresses the always-on debug coverage we ship for triage.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("app.input=debug"));
        assert!(DEFAULT_LOG_DIRECTIVES.contains("bridge.lifecycle=debug"));
        // `agent.env_git` is the target whose records were demoted out of
        // WARN; dropping it from this set would silence the git probe miss
        // entirely, which is the state the levels moved away from.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("agent.env_git=debug"));
        // `forge_server` pins the whole server crate at `debug`, so the
        // transport, surface and boot records ride it alongside the session
        // reads. Without the directive those records land nowhere at all.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("forge_server=debug"));
        // `forge_agent::userdata::catalog::scan` carries the scan's
        // module-path records at `debug`: an unparseable transcript line, a
        // session with no on-disk file, an anchor no row names. Each is one
        // session's own data rather than forge's health, and without the
        // directive the records land nowhere at all.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("forge_agent::userdata::catalog::scan=debug"));
        // `forge_workspace::output_tail` carries the tail read's
        // unreadable-line record, demoted there for the same reason: a line
        // the watched command wrote that cannot be decoded is that command's
        // own output. Without the directive the record never lands at all.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("forge_workspace::output_tail=debug"));
        // The seat's working-tree watch says why it holds nothing and why it
        // stops, both at `debug` because neither is forge's health. They are
        // the only record of a seat whose tree is silently not being read, so
        // the target needs the directive or the silence has no explanation.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("forge_workspace::work=debug"));
        // `forge_workspace::browser` records the browser calls a session
        // made - what ran, for whom, how it ended - at `debug` because a
        // call is the session's own work. Without the directive the call
        // record lands nowhere at all.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("forge_workspace::browser=debug"));
        // Dictation's own records were demoted out of WARN for the same
        // reason: a take reaching its capture cap, a normalizer hitting
        // its token budget and a take whose recognition failed are one
        // session's work, not forge's health. Without these two the
        // records land nowhere at all, which is the state the levels
        // moved away from. `forge_dictate` covers the leaf crate's sites
        // by module-path prefix; `forge_workspace::dictate` the seat
        // that runs a take.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("forge_workspace::dictate=debug"));
        assert!(DEFAULT_LOG_DIRECTIVES.contains("forge_dictate=debug"));
        // The `[server] enabled = false` record is a `debug` on
        // `app.lifecycle` because a config choice is not a problem, so
        // the target needs the directive or that record never lands.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("app.lifecycle=debug"));
        // llama-cpp emits per-token INFO/WARN chatter while a model runs.
        // Pinning to `error` rejects it at the filter before
        // serialisation.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("llama_cpp_2=error"));
        // The engine reaches tracing under both spellings of its target
        // (module path and hyphenated), so the pin has to ride both or the
        // chatter arrives under the one left out.
        assert!(DEFAULT_LOG_DIRECTIVES.contains("llama-cpp-2=error"));
        // A directive the parser rejects takes the whole filter with it
        // and leaves forge logging nothing, so the default set is
        // checked by parsing it rather than by reading it.
        assert!(
            tracing_subscriber::EnvFilter::try_new(DEFAULT_LOG_DIRECTIVES).is_ok(),
            "the default directives must parse",
        );
    }

    /// Emit one record through `directives` and answer what the sink
    /// caught. A directive names a target by module-path prefix, so the
    /// record a demoted site emits is the one its own module path names
    /// rather than the crate's. Measured rather than read: a directive
    /// that matches nothing reads exactly like one that works.
    fn emitted_under(directives: &str, emit: impl FnOnce()) -> String {
        use std::sync::{Arc, Mutex};

        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("the sink is not poisoned").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let written = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&written);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_env_filter(directives)
            .with_writer(move || Sink(Arc::clone(&sink)))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(written.lock().expect("the sink is not poisoned").clone())
            .expect("the sink holds utf-8")
    }

    /// The same, through the default set.
    fn emitted_under_defaults(emit: impl FnOnce()) -> String {
        emitted_under(DEFAULT_LOG_DIRECTIVES, emit)
    }

    /// The directive string boot builds for a preset, through the same
    /// `Cli` shape the binary passes.
    fn directives_under_preset(preset: &DiagnosticsPreset) -> String {
        build_filter_directives(&Cli {
            project: None,
            new: false,
            generate_completion: None,
            diagnostics_preset: Some(preset.clone()),
            log_file: None,
            log_filter: None,
        })
    }

    /// Driven through the site rather than through a target written here:
    /// a directive matches by prefix, so a record labelled by the test would
    /// prove only that the label matches. The reader's unreadable-line arm
    /// is the demoted one, and a line that is not valid UTF-8 is what
    /// reaches it.
    #[test]
    fn a_demoted_site_emits_under_the_default_directives() {
        let path =
            std::env::temp_dir().join(format!("forge-directive-check-{}.log", std::process::id()));
        std::fs::write(&path, b"one\ntwo\n\xff\xfe not utf-8\n").expect("write the probe file");

        let caught = emitted_under_defaults(|| {
            let _ = forge_server::output_tail::read_output_file_tail(&path, 12);
        });
        let _ = std::fs::remove_file(&path);

        assert!(
            caught.contains("output_tail_line_unreadable"),
            "the demoted record must land under the default directives: {caught}",
        );
    }

    /// The resolver composes a preset with the default set rather than
    /// letting the preset replace it: every default slot rides along under
    /// every preset, so a target that only the defaults name stays readable.
    /// `agent.env_git` is the sharp case - its records were demoted out of
    /// WARN, so without the directive they land nowhere at all - and it is
    /// in no preset's own list.
    #[test]
    fn a_preset_carries_the_default_set_rather_than_replacing_it() {
        let presets = [
            DiagnosticsPreset::Runtime,
            DiagnosticsPreset::Session,
            DiagnosticsPreset::Bridge,
            DiagnosticsPreset::Full,
        ];

        for preset in presets {
            let directives = directives_under_preset(&preset);
            for directive in DEFAULT_LOG_DIRECTIVES.split(',') {
                let key = directive_key(directive);
                assert!(
                    directives.split(',').any(|d| directive_key(d) == key),
                    "{preset:?} drops the default slot {directive}: {directives}",
                );
            }

            let caught = emitted_under(&directives, || {
                tracing::debug!(
                    target: "agent.env_git",
                    event_name = "preset_default_check",
                    "a record",
                );
            });
            assert!(
                caught.contains("preset_default_check"),
                "{preset:?} must keep a default-only target readable: {caught}",
            );
        }
    }

    /// The control for the composition: a preset's own directives still
    /// apply. `bridge.sdk` is the Bridge preset's to name - the defaults
    /// say nothing about it - so the record landing is the preset's doing.
    #[test]
    fn a_presets_own_directives_still_apply() {
        let directives = directives_under_preset(&DiagnosticsPreset::Bridge);

        let caught = emitted_under(&directives, || {
            tracing::debug!(
                target: "bridge.sdk",
                event_name = "preset_own_check",
                "a record",
            );
        });

        assert!(
            caught.contains("preset_own_check"),
            "the preset's own directives must survive the composition: {directives}",
        );
    }

    /// A slot both sides name keeps the preset's directive: no preset can be
    /// silently overridden by a default naming the same slot at a different
    /// level. Every overlap in the shipped sets is byte-identical, so the
    /// pairs here are synthetic - a divergent level is the case the rule
    /// exists for.
    #[test]
    fn a_preset_directive_wins_an_overlap_with_the_defaults() {
        // The target slot: the defaults name the same target at a lower level.
        let composed =
            compose_filter_directives("info,forge_server=trace", "info,forge_server=debug");

        let caught = emitted_under(&composed, || {
            tracing::trace!(
                target: "forge_server",
                event_name = "overlap_check",
                "a record",
            );
        });
        assert!(
            caught.contains("overlap_check"),
            "the preset's level must survive the target overlap: {composed}",
        );

        // The global slot: the parser resolves every bare level token to the
        // one global directive, so the defaults' trailing `info` would replace
        // the preset's `debug` if both rode the string.
        let composed = compose_filter_directives("debug,bridge.sdk=trace", "info,bridge.sdk=debug");

        let caught = emitted_under(&composed, || {
            tracing::debug!(
                target: "a_crate_forge_does_not_name::module",
                event_name = "global_overlap_check",
                "a record",
            );
        });
        assert!(
            caught.contains("global_overlap_check"),
            "the preset's baseline must survive the global overlap: {composed}",
        );
    }

    /// The key cuts at the `=` the parser would: one inside a brace shape
    /// (`[take{kind=open}]`) belongs to the shape, not to the directive, so
    /// an `open` shape and a `stop` shape key apart.
    #[test]
    fn a_directive_key_keeps_a_brace_shape_whole() {
        assert_eq!(directive_key("browser[take{kind=open}]=trace"), "browser[take{kind=open}]",);
        assert_ne!(
            directive_key("browser[take{kind=open}]=trace"),
            directive_key("browser[take{kind=stop}]=debug"),
        );
    }

    /// The control for `a_demoted_site_emits_under_the_default_directives`:
    /// a crate the defaults say nothing about emits nothing under them, so
    /// the record landing is the directive's doing rather than the
    /// subscriber's.
    #[test]
    fn a_target_the_defaults_say_nothing_about_stays_out() {
        let absent = emitted_under_defaults(|| {
            tracing::debug!(
                target: "a_crate_forge_does_not_name::module",
                event_name = "directive_check",
                "a record",
            );
        });

        assert!(absent.is_empty(), "no directive names this crate: {absent}");
    }

    /// The other half of that control: the baseline token is what decides
    /// whether a record no directive names lands at all, and it sits one
    /// character from `into` - which parses as a TRACE target of that
    /// name, leaving every specific directive passing its own records
    /// while the baseline is gone. One unread character, and the set goes
    /// quiet for everything unnamed. Measured over every set forge ships,
    /// not just the default one.
    #[test]
    fn every_shipped_filter_set_keeps_its_info_baseline() {
        let presets = [
            DiagnosticsPreset::Runtime,
            DiagnosticsPreset::Session,
            DiagnosticsPreset::Bridge,
            DiagnosticsPreset::Full,
        ];
        let sets = std::iter::once(("defaults".to_owned(), DEFAULT_LOG_DIRECTIVES.to_owned()))
            .chain(presets.into_iter().map(|p| (format!("{p:?}"), directives_under_preset(&p))));

        for (label, directives) in sets {
            let caught = emitted_under(&directives, || {
                tracing::info!(
                    target: "a_web_view::nowhere",
                    event_name = "baseline_check",
                    "a record no directive names",
                );
            });
            assert!(
                caught.contains("baseline_check"),
                "{label} has no working `info` baseline, so an unnamed target goes dark: {caught}",
            );
        }
    }

    #[test]
    fn resolve_log_path_uses_default_when_filter_enables_logging() {
        let cli = Cli {
            project: None,
            new: false,
            generate_completion: None,
            diagnostics_preset: None,
            log_file: None,
            log_filter: Some("app.render=trace".to_owned()),
        };

        let resolved = resolve_log_path(&cli).expect("resolve succeeds").expect("path exists");
        assert_eq!(resolved.source.as_str(), "default");
        let path = resolved.path.to_string_lossy().replace('\\', "/");
        assert!(path.ends_with("forge/logs/forge.log"));
    }

    #[test]
    fn resolve_log_path_uses_default_when_no_log_file() {
        let cli = Cli {
            project: None,
            new: false,
            generate_completion: None,
            diagnostics_preset: None,
            log_file: None,
            log_filter: None,
        };

        let resolved = resolve_log_path(&cli).expect("resolve succeeds").expect("path exists");
        assert_eq!(resolved.source.as_str(), "default");
    }

    #[test]
    fn resolve_log_path_uses_default_when_preset_is_set() {
        let cli = Cli {
            project: None,
            new: false,
            generate_completion: None,
            diagnostics_preset: Some(DiagnosticsPreset::Session),
            log_file: None,
            log_filter: None,
        };

        let resolved = resolve_log_path(&cli).expect("resolve succeeds").expect("path exists");
        assert_eq!(resolved.source.as_str(), "default");
    }

    #[test]
    fn rolling_writer_rotates_by_size() {
        let dir = tempdir().expect("temp dir");
        let base = dir.path().join("runtime.log");
        let mut writer = RollingFileWriter::new(&base, false, 10, 2).expect("writer");

        writer.write_all(b"12345").expect("first write");
        writer.write_all(b"67890").expect("second write");
        writer.write_all(b"abc").expect("rotation write");
        writer.flush().expect("flush");

        let current = fs::read_to_string(&base).expect("current log");
        let rotated = fs::read_to_string(rotated_log_path(&base, 1)).expect("rotated log");

        assert_eq!(current, "abc");
        assert_eq!(rotated, "1234567890");
    }

    #[test]
    fn rolling_writer_discards_the_oldest_beyond_max_files() {
        let dir = tempdir().expect("temp dir");
        let base = dir.path().join("runtime.log");
        let mut writer = RollingFileWriter::new(&base, false, 10, 2).expect("writer");

        for chunk in [b"aaaaa", b"bbbbb", b"ccccc", b"ddddd", b"eeeee", b"fffff", b"ggggg"] {
            writer.write_all(chunk).expect("write");
        }
        writer.flush().expect("flush");

        assert_eq!(fs::read_to_string(&base).expect("current log"), "ggggg");
        assert_eq!(fs::read_to_string(rotated_log_path(&base, 1)).expect("first"), "eeeeefffff");
        assert_eq!(fs::read_to_string(rotated_log_path(&base, 2)).expect("second"), "cccccddddd");
        assert!(
            !rotated_log_path(&base, 3).exists(),
            "the window must stay capped at max_files, dropping the oldest"
        );
    }

    #[test]
    fn clear_rotated_files_removes_existing_window() {
        let dir = tempdir().expect("temp dir");
        let base = dir.path().join("runtime.log");
        fs::write(rotated_log_path(&base, 1), "a").expect("write first");
        fs::write(rotated_log_path(&base, 2), "b").expect("write second");

        clear_rotated_files(&base, 2).expect("clear rotated files");

        assert!(!rotated_log_path(&base, 1).exists());
        assert!(!rotated_log_path(&base, 2).exists());
    }
}
