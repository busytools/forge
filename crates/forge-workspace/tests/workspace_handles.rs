//! `Workspace::get_agent_handle` integration tests - verify the
//! cross-crate plumbing from `forge.toml` through the account
//! selection into the spawned `AgentHandle`'s bound `config_dir`. No real
//! `claude` subprocesses are spawned; the test asserts up to the
//! `AgentHandle`/`Bridge` boundary, where the bridge's typed
//! `config_dir` field is the source of truth (read by every
//! in-process accessor and exported as `CLAUDE_CONFIG_DIR` to the
//! spawned subprocess).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use forge_workspace::protocol::SpawnRole;
use forge_workspace::{SessionLaunchSettings, SessionTarget, Workspace};
use tempfile::tempdir;

/// Ensure `forge/` exists and return the production `forge/forge.toml`
/// path, so tests write where forge reads (not the legacy fallback).
fn forge_toml_path(config_dir: &std::path::Path) -> PathBuf {
    let forge = config_dir.join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    forge.join("forge.toml")
}

#[tokio::test]
async fn account_display_name_reaches_bridge() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        forge_toml_path(dir.path()),
        r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"

[[orgs.projects]]
name = "dotfiles"
path = "~/Projects/dotfiles"
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Gateway"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
    workspace.seed_test_ready_account("Stargate");

    // Both spawns walk the pin and take the first ready account. The
    // important assertion here is that the bridge actually carries
    // a display_name through to the AgentHandle.
    let h1 = workspace
        .get_agent_handle(
            SessionTarget::Default,
            SessionLaunchSettings::default(),
            &SpawnRole::Lead,
        )
        .expect("first spawn");
    assert_eq!(
        h1.display_name().as_deref(),
        Some("Stargate"),
        "first spawn binds to Stargate's display_name (first in the pin)",
    );

    let h2 = workspace
        .get_agent_handle(
            // A named target is a project's lead.
            SessionTarget::Named("dotfiles".to_owned()),
            SessionLaunchSettings::default(),
            &SpawnRole::Lead,
        )
        .expect("second spawn");
    assert_eq!(
        h2.display_name().as_deref(),
        Some("Stargate"),
        "the walk has no per-spawn spread, so the second spawn takes Stargate too",
    );
}

/// The wiring itself: everything else stops at the resolution helper's
/// return value, so replacing the one line that applies it at the spawn
/// site passes every unit test. This asserts on what the handle
/// actually carries.
///
/// `Named` is the arm production takes - every `auto_start = true`
/// project and every `--project NAME` reaches it. It also covers
/// `[accounts.env]`, which had no end-to-end coverage of its own.
#[tokio::test]
async fn declared_env_reaches_the_spawned_handle() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        forge_toml_path(dir.path()),
        r#"
[env]
GLOBAL_KEY = "global-value"

[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true
model = "claude-sonnet-5"
env = { AIRMAIL_TOKEN = "forge-value" }

[[orgs.projects]]
name = "airmail"
path = "~/Projects/airmail"
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
[accounts.env]
ACCOUNT_KEY = "account-value"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
    workspace.seed_test_ready_account("Stargate");

    let handle = workspace
        .get_agent_handle(
            // A named target is a project's lead.
            SessionTarget::Named("forge".to_owned()),
            SessionLaunchSettings::default(),
            &SpawnRole::Lead,
        )
        .expect("spawn forge");
    let env = handle.env();
    assert_eq!(
        env.get("AIRMAIL_TOKEN").map(String::as_str),
        Some("forge-value"),
        "the project's declared env has to reach the handle, not just the helper",
    );
    assert_eq!(
        env.get("ACCOUNT_KEY").map(String::as_str),
        Some("account-value"),
        "[accounts.env] reaches the handle too",
    );
    assert_eq!(
        env.get("GLOBAL_KEY").map(String::as_str),
        Some("global-value"),
        "and the global [env] base",
    );

    let other = workspace
        .get_agent_handle(
            // A named target is a project's lead.
            SessionTarget::Named("airmail".to_owned()),
            SessionLaunchSettings::default(),
            &SpawnRole::Lead,
        )
        .expect("spawn airmail");
    let other_env = other.env();
    assert!(
        !other_env.contains_key("AIRMAIL_TOKEN"),
        "a second project on the same account must not receive it: {other_env:?}",
    );
    assert_eq!(
        other_env.get("ACCOUNT_KEY").map(String::as_str),
        Some("account-value"),
        "while still getting the account's own",
    );
}
