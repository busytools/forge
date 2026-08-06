//! Capture the real tracing records a spawn emits and assert on them.
//!
//! The spawn log is always-on INFO. `applied_env_keys` being correct is
//! not load-bearing when nothing pins the call site, and the call site
//! is what a future edit touches: adding `env = ?project.env` to the
//! `info!` writes every project's token VALUES into the log and passes
//! every other test in the workspace.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use forge_workspace::{SessionLaunchSettings, SessionTarget, Workspace};
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

fn forge_toml_path(config_dir: &std::path::Path) -> PathBuf {
    let forge = config_dir.join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    forge.join("forge.toml")
}

const TOKEN: &str = "tok-must-never-be-logged";
const ENDPOINT: &str = "https://project-endpoint.invalid";

const FIXTURE: &str = r#"
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

[projects.forge.env]
BUSYMAIL_TOKEN = "tok-must-never-be-logged"
ANTHROPIC_BASE_URL = "https://project-endpoint.invalid"
"#;

#[tokio::test]
async fn spawn_logs_key_names_and_never_a_declared_value() {
    let capture = Capture::default();
    // TRACE and process-wide, not INFO and thread-local. At the default
    // level this saw only the two records inside `session_env_for`, so a
    // `debug!` carrying values right beside the guarded `info!` - or any
    // record emitted off the spawning thread - passed unnoticed. nextest
    // gives each test its own process, so a global default is safe.
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(capture.clone())
            .finish(),
    )
    .expect("install capture subscriber");

    let dir = tempdir().expect("tempdir");
    fs::write(forge_toml_path(dir.path()), FIXTURE).expect("write forge.toml");
    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
    workspace
        .get_agent_handle(
            SessionTarget::Named("forge".to_owned()),
            SessionLaunchSettings::default(),
        )
        .expect("spawn forge");
    // Let any off-thread emitter land before reading the buffer.
    tokio::task::yield_now().await;

    let log = capture.text();
    assert!(log.contains("session_env_project_applied"), "the per-spawn record is emitted: {log}");
    assert!(log.contains("BUSYMAIL_TOKEN"), "key names are recorded: {log}");
    assert!(
        !log.contains(TOKEN),
        "a declared VALUE reached the log - this is the regression: {log}",
    );
    assert!(!log.contains(ENDPOINT), "nor an endpoint value: {log}");
    assert!(
        log.contains("session_env_project_overrides_endpoint"),
        "and the accounting-desync warn fires for a project endpoint key: {log}",
    );
}
