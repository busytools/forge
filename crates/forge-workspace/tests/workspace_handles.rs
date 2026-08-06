//! `Workspace::get_agent_handle` integration tests - verify the
//! cross-crate plumbing from `forge.toml` through the account picker
//! into the spawned `AgentHandle`'s bound `config_dir`. No real
//! `claude` subprocesses are spawned; the test asserts up to the
//! `AgentHandle`/`Bridge` boundary, where the bridge's typed
//! `config_dir` field is the source of truth (read by every
//! in-process accessor and exported as `CLAUDE_CONFIG_DIR` to the
//! spawned subprocess).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use forge_workspace::{SessionKey, SessionLaunchSettings, SessionTarget, Workspace};
use tempfile::tempdir;

/// Ensure `forge/` exists and return the production `forge/forge.toml`
/// path, so tests write where forge reads (not the legacy fallback).
fn forge_toml_path(config_dir: &std::path::Path) -> PathBuf {
    let forge = config_dir.join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    forge.join("forge.toml")
}

#[tokio::test]
async fn cold_cache_dual_spawns_rotate_across_allow_list() {
    // Round-robin cursor advances per pick - under a cold usage
    // cache (both accounts in tier 0 / Usable), the first spawn
    // picks Subspace (cursor=0 → first allow-list entry) and the
    // second rotates to Granite (cursor=1). Spreads load across
    // healthy accounts instead of always hammering the first.
    let dir = tempdir().expect("tempdir");
    fs::write(
        forge_toml_path(dir.path()),
        r#"
[[orgs]]
name = "Default"
accounts = ["Subspace", "Granite"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/forge-test-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/forge-test-granite"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

    let h1 = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .expect("first spawn");
    assert_eq!(
        h1.config_dir(),
        PathBuf::from("/tmp/forge-test-subspace"),
        "first spawn (cursor=0) binds to Subspace's config_dir (first usable in allow-list)",
    );

    // Second spawn under a distinct SessionTarget - same allow-list,
    // cursor advances to 1, rotates to Granite.
    let other = SessionKey::from_str_for_test("dual-account-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .expect("second spawn");
    assert_eq!(
        h2.config_dir(),
        PathBuf::from("/tmp/forge-test-granite"),
        "second spawn (cursor=1) rotates to Granite's config_dir (round-robin)",
    );
}

#[tokio::test]
async fn picker_display_name_reaches_bridge() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        forge_toml_path(dir.path()),
        r#"
[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/forge-test-display-subspace"

[[accounts]]
display_name = "Granite"
config_dir = "/tmp/forge-test-display-granite"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

    // Cold cache → both spawns pick Subspace (first in pin). The
    // important assertion here is that the bridge actually carries
    // a display_name through to the AgentHandle.
    let h1 = workspace
        .get_agent_handle(SessionTarget::Default, SessionLaunchSettings::default())
        .expect("first spawn");
    assert_eq!(
        h1.display_name().as_deref(),
        Some("Subspace"),
        "first spawn binds to Subspace's display_name (first in pin, cold cache)",
    );

    let other = SessionKey::from_str_for_test("display-name-other");
    let h2 = workspace
        .get_agent_handle(SessionTarget::Session(other), SessionLaunchSettings::default())
        .expect("second spawn");
    assert_eq!(
        h2.display_name().as_deref(),
        Some("Subspace"),
        "second spawn also binds to Subspace under cold cache",
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
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[orgs.projects]]
name = "busymail"
path = "~/Projects/busymail"

[[accounts]]
display_name = "Subspace"
config_dir = "/tmp/forge-test-env-subspace"
[accounts.env]
ACCOUNT_KEY = "account-value"

[projects.forge.env]
BUSYMAIL_TOKEN = "forge-value"
"#,
    )
    .expect("write forge.toml");

    let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));

    let handle = workspace
        .get_agent_handle(
            SessionTarget::Named("forge".to_owned()),
            SessionLaunchSettings::default(),
        )
        .expect("spawn forge");
    let env = handle.env();
    assert_eq!(
        env.get("BUSYMAIL_TOKEN").map(String::as_str),
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
            SessionTarget::Named("busymail".to_owned()),
            SessionLaunchSettings::default(),
        )
        .expect("spawn busymail");
    let other_env = other.env();
    assert!(
        !other_env.contains_key("BUSYMAIL_TOKEN"),
        "a second project on the same account must not receive it: {other_env:?}",
    );
    assert_eq!(
        other_env.get("ACCOUNT_KEY").map(String::as_str),
        Some("account-value"),
        "while still getting the account's own",
    );
}
