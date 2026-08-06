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

/// Everything except the one record known to leak today.
///
/// `forge-sdk`'s spawn path logs the `Command` at DEBUG, and
/// `Command`'s `Debug` renders every env var as `KEY="value"` - so the
/// last hop before exec prints the whole merged environment. That is a
/// live leak on main, fixed by #564, and it is outside this crate.
/// Excluding exactly that record keeps this guard failing for any OTHER
/// leak instead of going red on a known one.
///
/// Paired with [`assert_the_exclusion_is_still_needed`] so it expires
/// on its own: the moment #564 lands and that line stops carrying
/// values, this filter starts hiding nothing and the assertion goes red
/// rather than waiting for someone to read a comment.
fn without_the_known_sdk_leak(log: &str) -> String {
    log.lines().filter(|line| !line.contains(SPAWN_RECORD)).collect::<Vec<_>>().join("\n")
}

const SPAWN_RECORD: &str = r#""message":"spawning claude subprocess""#;

/// Fails once the excluded record stops leaking - which is #564's
/// acceptance test, enforced rather than documented.
fn assert_the_exclusion_is_still_needed(log: &str) {
    let spawn = log
        .lines()
        .find(|line| line.contains(SPAWN_RECORD))
        .expect("no spawn record: delete without_the_known_sdk_leak and this assertion");
    assert!(
        SECRETS.iter().any(|secret| spawn.contains(secret)),
        "the spawn record no longer carries declared values, so #564 has landed - delete \
         without_the_known_sdk_leak, its call sites and this assertion: {spawn}",
    );
}

/// The single record carrying `event_name`, so level and fields are
/// asserted on the same line rather than anywhere in the capture.
fn record<'a>(log: &'a str, event_name: &str) -> &'a str {
    let needle = format!("\"event_name\":\"{event_name}\"");
    log.lines()
        .find(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("no {event_name} record in:\n{log}"))
}

/// Wait for the log to go quiet rather than yielding once. An absence
/// assertion cannot poll for what must not appear, so it has to outlast
/// the emitters: a value leaked onto a background task lands tens of
/// milliseconds after the spawn call returns, and `yield_now` does not
/// wait for it - which made every absence assertion here a race that
/// degraded silently in the passing direction.
async fn settled(capture: &Capture) -> String {
    let mut last = capture.text();
    let mut quiet_rounds = 0;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let now = capture.text();
        quiet_rounds = if now.len() == last.len() { quiet_rounds + 1 } else { 0 };
        last = now;
        if quiet_rounds >= 3 {
            break;
        }
    }
    last
}

async fn spawn_with_capture(target: SessionTarget) -> String {
    let capture = install_capture();
    let dir = tempdir().expect("tempdir");
    fs::write(forge_toml_path(dir.path()), FIXTURE).expect("write forge.toml");
    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
    workspace.get_agent_handle(target, SessionLaunchSettings::default()).expect("spawn");
    settled(&capture).await
}

#[tokio::test]
async fn spawn_logs_key_names_and_never_a_declared_value() {
    let log = spawn_with_capture(SessionTarget::Named("forge".to_owned())).await;

    let applied = record(&log, "session_env_project_applied");
    // Level asserted on the record: these are the only diagnostics for
    // which project's env a session got, and `forge_workspace::workspace`
    // is not in the default debug directives, so a downgrade to `debug!`
    // deletes them from the real log file while every test still passes.
    assert!(applied.contains(r#""level":"INFO""#), "the spawn record stays at INFO: {applied}");
    assert!(applied.contains(r#""project":"forge""#), "and names the project: {applied}");
    assert!(applied.contains("BUSYMAIL_TOKEN"), "key names are recorded: {applied}");
    assert_the_exclusion_is_still_needed(&log);
    let guarded = without_the_known_sdk_leak(&log);
    for secret in SECRETS {
        assert!(
            !guarded.contains(secret),
            "a declared VALUE reached the log: {secret} in {guarded}"
        );
    }
    assert!(!guarded.contains(GLOBAL_SECRET), "nor a global [env] value: {guarded}");
    // Counted, not merely present: with one endpoint key in the fixture
    // and a presence assertion, shrinking the guarded key list to one
    // entry passed.
    assert_eq!(
        log.matches("session_env_project_overrides_endpoint").count(),
        2,
        "the accounting-desync warn fires once per project endpoint key: {log}",
    );
}

#[tokio::test]
async fn an_unresolved_spawn_target_warns() {
    let log = spawn_with_capture(SessionTarget::Session(SessionKey::from_str_for_test(
        "no-such-session",
    )))
    .await;

    let unresolved = record(&log, "session_env_project_unresolved");
    assert!(
        unresolved.contains(r#""level":"WARN""#),
        "an orphan target warns, and stays at WARN: {unresolved}",
    );
    assert!(unresolved.contains("spawn_target"), "and carries the target field: {unresolved}");
    let guarded = without_the_known_sdk_leak(&log);
    for secret in SECRETS {
        assert!(!guarded.contains(secret), "still no declared value: {secret} in {guarded}");
    }
    // The unresolved path returns global + account, so the global layer
    // is the one this test most needs to guard.
    assert!(!guarded.contains(GLOBAL_SECRET), "nor a global [env] value: {guarded}");
}
