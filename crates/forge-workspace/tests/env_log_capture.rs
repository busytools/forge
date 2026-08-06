//! Capture the real tracing records a spawn emits and assert on them.
//!
//! The spawn log is always-on. `applied_env_keys` being correct is not
//! load-bearing when nothing pins the call site, and the call site is
//! what a future edit touches: adding `env = ?project.env` to the
//! `info!` writes every project's token VALUES into the log and passes
//! every other test in the workspace.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use forge_workspace::{SessionKey, SessionLaunchSettings, SessionTarget, Workspace};
use tempfile::tempdir;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("capture lock")).into_owned()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// TRACE and process-wide. At the default INFO this saw only the two
/// records inside `session_env_for`, so a `debug!` beside the guarded
/// `info!` passed unnoticed - and the default directives put
/// `bridge.lifecycle=debug`, so below-INFO records do reach the real
/// log file. `set_default` is thread-local and would miss anything
/// emitted off the spawning thread. nextest is process-per-test, so
/// the once-per-process limit is fine.
fn install_capture() -> Capture {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(capture.clone())
            .finish(),
    )
    .expect(
        "a global subscriber is already installed - this harness needs one process per test, \
         which nextest provides and `cargo test` does not",
    );
    capture
}

/// Whether a `claude` binary is on PATH.
///
/// `Client::spawn` runs a `--version` probe before building the command,
/// and that probe is a fork+exec which fails when the binary is absent,
/// aborting before the spawn record this harness watches. So without a
/// CLI the capture collapses to ~10 lines and covers no spawn path.
///
/// A `--version` shim would be enough to get past the probe - verified -
/// but the harness cannot install one itself: `std::env::set_var` is
/// unsafe in this edition and this workspace builds with `-F
/// unsafe-code`, which a local `allow` cannot override. So the shim
/// would have to come from outside the process (a CI step), which is a
/// workflow decision rather than something this test can take.
fn claude_on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("claude").is_file()))
}

/// Announce loudly rather than pass quietly. A capture without the spawn
/// path asserts almost nothing, and the anchor below exists precisely
/// because a shorter log is an ABSENT assertion rather than a weaker one -
/// so degrading to a silent pass here would reintroduce it.
fn skip_without_claude(test: &str) -> bool {
    if claude_on_path() {
        return false;
    }
    eprintln!(
        "SKIPPING {test}: no `claude` on PATH, so the spawn never reaches the subprocess and \
         this harness cannot cover the path it exists to guard. Not a pass."
    );
    true
}

fn forge_toml_path(config_dir: &std::path::Path) -> PathBuf {
    let forge = config_dir.join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    forge.join("forge.toml")
}

/// One distinctively-valued key per layer. A regression that dumps the
/// merged map, or any single layer of it, has to trip an absence
/// assertion whichever layer it read - and `[accounts.env]` is where
/// the real `ANTHROPIC_AUTH_TOKEN` lives, so leaving that layer
/// unvalued would have guarded everything except the credential.
const SECRETS: [&str; 4] = [
    "tok-must-never-be-logged",
    "https://project-endpoint.invalid",
    "auth-tok-must-never-be-logged",
    "account-secret-must-never-be-logged",
];
const GLOBAL_SECRET: &str = "global-secret-must-never-be-logged";

/// Same shape with no `[projects.<name>.env]` at all - the case the
/// applied record exists to cover, since a target resolving to a
/// project you did not mean is only visible if the record fires when
/// that project declares nothing.
const FIXTURE_NO_PROJECT_ENV: &str = r#"
[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/forge-test-logcap-noenv"
"#;

const FIXTURE: &str = r#"
[env]
GLOBAL_KEY = "global-secret-must-never-be-logged"

[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/forge-test-logcap-subspace"
[accounts.env]
ACCOUNT_KEY = "account-secret-must-never-be-logged"

[projects.forge.env]
BUSYMAIL_TOKEN = "tok-must-never-be-logged"
ANTHROPIC_BASE_URL = "https://project-endpoint.invalid"
ANTHROPIC_AUTH_TOKEN = "auth-tok-must-never-be-logged"
"#;

/// The single record carrying `event_name`, so level and fields are
/// asserted on the same line rather than anywhere in the capture.
fn record<'a>(log: &'a str, event_name: &str) -> &'a str {
    let needle = format!("\"event_name\":\"{event_name}\"");
    log.lines()
        .find(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("no {event_name} record in:\n{log}"))
}

/// Wait until `anchor` - a tracing target the spawn must reach - has
/// appeared, then for the log to go quiet. A missing anchor FAILS.
///
/// Two things this replaces. Quiescence alone was a heuristic tuned to
/// an incidental gap in this pipeline: three quiet 50ms samples happen
/// to fall between two waves, so leaks deferred past ~1.5s survived it.
/// And the capture silently loses depth by environment - with `claude`
/// absent from PATH it collapses from ~56 lines to ~10, which is 82% of
/// the surface gone with every assertion still passing, and the missing
/// part is exactly where #564's leak lived. Anchoring on a target makes
/// that collapse a failure instead of a quieter pass.
async fn settled_after(capture: &Capture, anchor: &str) -> String {
    let mut last = capture.text();
    let mut quiet_rounds = 0;
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let now = capture.text();
        let grew = now.len() != last.len();
        last = now;
        quiet_rounds = if grew { 0 } else { quiet_rounds + 1 };
        if last.contains(anchor) && quiet_rounds >= 3 {
            return last;
        }
    }
    assert!(
        last.contains(anchor),
        "the spawn never reached {anchor}, so this capture covers less than it claims \
         ({} lines). A shorter log is not a weaker assertion, it is an absent one: {last}",
        last.lines().count(),
    );
    last
}

/// A resolved target spawns the in-process MCP server, so this target
/// only appears when the spawn got that far.
const RESOLVED_ANCHOR: &str = "forge_sdk::mcp::server";
/// An unresolved target still reaches the subprocess spawn, but never
/// the MCP server - so it needs the shallower anchor.
const UNRESOLVED_ANCHOR: &str = "forge_sdk::transport::process";

async fn spawn_with_capture(target: SessionTarget, anchor: &str) -> String {
    spawn_with_fixture(target, FIXTURE, anchor).await
}

async fn spawn_with_fixture(target: SessionTarget, fixture: &str, anchor: &str) -> String {
    let capture = install_capture();
    let dir = tempdir().expect("tempdir");
    fs::write(forge_toml_path(dir.path()), fixture).expect("write forge.toml");
    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
    workspace.get_agent_handle(target, SessionLaunchSettings::default()).expect("spawn");
    settled_after(&capture, anchor).await
}

#[tokio::test]
async fn spawn_logs_key_names_and_never_a_declared_value() {
    if skip_without_claude("spawn_logs_key_names_and_never_a_declared_value") {
        return;
    }
    let log = spawn_with_capture(SessionTarget::Named("forge".to_owned()), RESOLVED_ANCHOR).await;

    let applied = record(&log, "session_env_project_applied");
    // Level asserted on the record: these are the only diagnostics for
    // which project's env a session got, and `forge_workspace::workspace`
    // is not in the default debug directives, so a downgrade to `debug!`
    // deletes them from the real log file while every test still passes.
    assert!(applied.contains(r#""level":"INFO""#), "the spawn record stays at INFO: {applied}");
    assert!(applied.contains(r#""project":"forge""#), "and names the project: {applied}");
    assert!(
        applied.contains(r#""target":"forge_workspace::workspace""#),
        "emitted from the workspace target: {applied}",
    );
    assert!(applied.contains("BUSYMAIL_TOKEN"), "key names are recorded: {applied}");
    for secret in SECRETS {
        assert!(!log.contains(secret), "a declared VALUE reached the log: {secret} in {log}");
    }
    assert!(!log.contains(GLOBAL_SECRET), "nor a global [env] value: {log}");
    // Per-record, not a substring count. A count kills a SHORTENED key
    // list but misses a re-pointed one - `["ANTHROPIC_BASE_URL",
    // "ANTHROPIC_BASE_URL"]` still counts two - and says nothing about
    // the level, which is the consequential half: this target is absent
    // from DEFAULT_LOG_DIRECTIVES, whose root is `info`, so a
    // below-INFO record is deleted from the real log file.
    let desync: Vec<&str> = log
        .lines()
        .filter(|line| line.contains(r#""event_name":"session_env_project_overrides_endpoint""#))
        .collect();
    assert_eq!(desync.len(), 2, "one record per project endpoint key: {log}");
    for key in ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"] {
        assert!(
            desync.iter().any(|line| line.contains(&format!(r#""key":"{key}""#))),
            "a record names {key}: {desync:?}",
        );
    }
    for line in &desync {
        assert!(line.contains(r#""level":"WARN""#), "and each stays at WARN: {line}");
    }
}

#[tokio::test]
async fn an_unresolved_spawn_target_warns() {
    if skip_without_claude("an_unresolved_spawn_target_warns") {
        return;
    }
    let log = spawn_with_capture(
        SessionTarget::Session(SessionKey::from_str_for_test("no-such-session")),
        UNRESOLVED_ANCHOR,
    )
    .await;

    let unresolved = record(&log, "session_env_project_unresolved");
    assert!(
        unresolved.contains(r#""level":"WARN""#),
        "an orphan target warns, and stays at WARN: {unresolved}",
    );
    assert!(
        unresolved.contains("no-such-session"),
        "and carries the actual target, not just the field name: {unresolved}",
    );
    assert!(
        unresolved.contains(r#""target":"forge_workspace::workspace""#),
        "emitted from the workspace target: {unresolved}",
    );
    for secret in SECRETS {
        assert!(!log.contains(secret), "still no declared value: {secret} in {log}");
    }
    // The unresolved path returns global + account, so the global layer
    // is the one this test most needs to guard.
    assert!(!log.contains(GLOBAL_SECRET), "nor a global [env] value: {log}");
}

/// The record is unconditional by design, and that is the half nothing
/// else pins: gating the `info!` on the project declaring env passes
/// every other test, and takes with it the only signal that says which
/// project a session actually resolved to.
#[tokio::test]
async fn the_applied_record_fires_for_a_project_declaring_no_env() {
    if skip_without_claude("the_applied_record_fires_for_a_project_declaring_no_env") {
        return;
    }
    let log = spawn_with_fixture(
        SessionTarget::Named("forge".to_owned()),
        FIXTURE_NO_PROJECT_ENV,
        RESOLVED_ANCHOR,
    )
    .await;

    let applied = record(&log, "session_env_project_applied");
    assert!(applied.contains(r#""project":"forge""#), "names the resolved project: {applied}");
    assert!(applied.contains(r#""keys":"""#), "with an empty key list, not a missing record");
}

/// The WARN's gate suppresses it when NO project declares env, so the
/// control has to be an unresolved target in a config that declares
/// none - a resolved spawn never reaches the gate at all, and asserting
/// on one pins nothing.
#[tokio::test]
async fn the_unresolved_warn_is_silent_when_no_project_declares_env() {
    if skip_without_claude("the_unresolved_warn_is_silent_when_no_project_declares_env") {
        return;
    }
    let log = spawn_with_fixture(
        SessionTarget::Session(SessionKey::from_str_for_test("no-such-session")),
        FIXTURE_NO_PROJECT_ENV,
        UNRESOLVED_ANCHOR,
    )
    .await;
    // Positive anchor first: an absence assertion over an empty capture
    // passes for the wrong reason, and this test is all absence.
    assert!(
        log.contains("session_env_project_applied") || log.contains(UNRESOLVED_ANCHOR),
        "the spawn produced records at all, so the absence below means something: {log}",
    );
    assert!(
        !log.contains("session_env_project_unresolved"),
        "nothing to misroute, so the warn is noise here: {log}",
    );
}
