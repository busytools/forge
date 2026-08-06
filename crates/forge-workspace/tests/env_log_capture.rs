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
    .expect("install capture subscriber");
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

async fn spawn_with_capture(target: SessionTarget) -> String {
    let capture = install_capture();
    let dir = tempdir().expect("tempdir");
    fs::write(forge_toml_path(dir.path()), FIXTURE).expect("write forge.toml");
    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
    workspace.get_agent_handle(target, SessionLaunchSettings::default()).expect("spawn");
    // Let any off-thread emitter land before reading the buffer.
    tokio::task::yield_now().await;
    capture.text()
}

#[tokio::test]
async fn spawn_logs_key_names_and_never_a_declared_value() {
    let log = spawn_with_capture(SessionTarget::Named("forge".to_owned())).await;

    assert!(log.contains("session_env_project_applied"), "the per-spawn record is emitted: {log}");
    assert!(log.contains("BUSYMAIL_TOKEN"), "key names are recorded: {log}");
    for secret in SECRETS {
        assert!(!log.contains(secret), "a declared VALUE reached the log: {secret} in {log}");
    }
    assert!(!log.contains(GLOBAL_SECRET), "nor a global [env] value: {log}");
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

    assert!(
        log.contains("session_env_project_unresolved"),
        "an orphan target has to warn, not fall through silently: {log}",
    );
    assert!(log.contains("spawn_target"), "and carry the target field: {log}");
    for secret in SECRETS {
        assert!(!log.contains(secret), "still no declared value: {secret} in {log}");
    }
}
