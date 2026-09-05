//! `/account` mid-session switch integration tests. Verify that
//! `Command::SwitchAccount` re-spawns the SAME session key under the
//! picked account's `config_dir` (forcing the account, not letting the
//! round-robin picker choose), and that the server-side backstop
//! refuses a switch while a turn is in flight. Mirror `workspace_handles`:
//! assert at the `AgentHandle`/bridge `config_dir` boundary, no real
//! `claude` conversation required.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use forge_workspace::{
    Command, RuntimeSessionState, SessionKey, SessionLaunchSettings, SessionTarget, SessionUpdate,
    Workspace,
};
use tempfile::tempdir;

/// Write a three-account (`Aacct` / `Bacct` / `Cacct`) forge.toml into
/// `config_dir` and return the workspace. Cold-cache picks rotate in
/// definition order: cursor=0 -> Aacct, cursor=1 -> Bacct, ...
fn three_account_workspace(dir: &std::path::Path) -> Arc<Workspace> {
    let forge = dir.join("forge");
    fs::create_dir_all(&forge).expect("forge/ dir");
    fs::write(
        forge.join("forge.toml"),
        r#"
[[orgs]]
name = "Default"
accounts = ["Aacct", "Bacct", "Cacct"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Aacct"
config_dir = "/tmp/forge-switch-a"
provider = "anthropic"

[[accounts]]
display_name = "Bacct"
config_dir = "/tmp/forge-switch-b"
provider = "anthropic"

[[accounts]]
display_name = "Cacct"
config_dir = "/tmp/forge-switch-c"
provider = "anthropic"
"#,
    )
    .expect("write forge.toml");
    Arc::new(Workspace::new_for_test(dir.to_owned()).expect("new"))
}

#[tokio::test]
async fn switch_account_respawns_same_session_under_forced_config_dir() {
    // Three accounts. A cold-cache initial spawn takes cursor=0 (Aacct);
    // an UNforced re-spawn would advance the round-robin to cursor=1
    // (Bacct). The switch targets Cacct, so landing on Cacct's
    // config_dir proves the account was FORCED, not merely rotated to.
    let dir = tempdir().expect("tempdir");
    let workspace = three_account_workspace(dir.path());

    // Initial spawn: cold cache -> first usable in the pin (Aacct).
    let key = SessionKey::from_str_for_test("switch-target");
    let handle = workspace
        .get_agent_handle(SessionTarget::Session(key.clone()), SessionLaunchSettings::default())
        .expect("initial spawn");
    assert_eq!(
        handle.config_dir(),
        PathBuf::from("/tmp/forge-switch-a"),
        "initial spawn binds to account A (first usable, cold cache)",
    );

    // Switch to Cacct - the account the round-robin would skip past.
    workspace
        .dispatch(Command::SwitchAccount {
            key: key.clone(),
            account_display_name: "Cacct".to_owned(),
            launch_settings: SessionLaunchSettings::default(),
        })
        .expect("dispatch switch");

    // Same key, re-spawned under the FORCED account C's config_dir.
    assert_eq!(
        workspace.config_dir_for(&key),
        Some(PathBuf::from("/tmp/forge-switch-c")),
        "switch re-spawns the SAME session key under the forced account C's config_dir",
    );
    assert!(
        workspace.has_agent_for(&key),
        "the re-spawned session keeps its live agent under the same key",
    );
}

#[tokio::test]
async fn switch_account_refused_while_a_turn_is_in_flight() {
    // The authoritative backstop: a delivered peer / cron / gotify prompt
    // can start a turn between picker-open and Enter. handle_switch_account
    // must refuse (notice, no teardown) rather than tear down the live turn.
    let dir = tempdir().expect("tempdir");
    let workspace = three_account_workspace(dir.path());
    let mut updates = workspace.subscribe().expect("subscribe");

    let key = SessionKey::from_str_for_test("switch-busy");
    workspace
        .get_agent_handle(SessionTarget::Session(key.clone()), SessionLaunchSettings::default())
        .expect("initial spawn");
    assert_eq!(workspace.config_dir_for(&key), Some(PathBuf::from("/tmp/forge-switch-a")));

    // A turn is now in flight for this session.
    workspace.domain_session_for(&key).expect("domain").lock().runtime_state =
        Some(RuntimeSessionState::Running);

    workspace
        .dispatch(Command::SwitchAccount {
            key: key.clone(),
            account_display_name: "Cacct".to_owned(),
            launch_settings: SessionLaunchSettings::default(),
        })
        .expect("dispatch switch");

    // Refused: the session stays on account A and keeps its live agent.
    assert_eq!(
        workspace.config_dir_for(&key),
        Some(PathBuf::from("/tmp/forge-switch-a")),
        "a busy session is NOT switched",
    );
    assert!(workspace.has_agent_for(&key), "the in-flight session is NOT torn down");

    // The idle notice was surfaced.
    let mut saw_notice = false;
    while let Ok(update) = updates.try_recv() {
        if let SessionUpdate::SlashCommandError { message, .. } = update
            && message.contains("Finish or cancel")
        {
            saw_notice = true;
        }
    }
    assert!(saw_notice, "a busy switch surfaces the idle notice");
}

#[tokio::test]
async fn switch_account_refused_when_a_prompt_is_routed_before_the_wire_echo() {
    // The wire-lag window the runtime_state mirror alone misses: a Prompt
    // is committed (routed) but the CLI hasn't echoed
    // session_state_changed=Running yet, so runtime_state is still
    // unmirrored. Only the synchronous turn_pending marker catches it -
    // the switch must refuse on turn_pending alone, without teardown.
    let dir = tempdir().expect("tempdir");
    let workspace = three_account_workspace(dir.path());
    let mut updates = workspace.subscribe().expect("subscribe");

    let key = SessionKey::from_str_for_test("switch-prompt-race");
    workspace
        .get_agent_handle(SessionTarget::Session(key.clone()), SessionLaunchSettings::default())
        .expect("initial spawn");
    assert_eq!(workspace.config_dir_for(&key), Some(PathBuf::from("/tmp/forge-switch-a")));

    // Route a Prompt: turn_pending is stamped synchronously, before any
    // Running echo could be mirrored.
    workspace
        .dispatch(Command::Prompt {
            key: key.clone(),
            text: "hi".to_owned(),
            attachments: Vec::new(),
        })
        .expect("dispatch prompt");
    {
        let domain = workspace.domain_session_for(&key).expect("domain");
        let guard = domain.lock();
        assert!(guard.turn_pending, "routing a Prompt stamps turn_pending synchronously");
        assert!(
            guard.runtime_state.is_none(),
            "runtime_state is still unmirrored - only turn_pending covers this window",
        );
    }

    workspace
        .dispatch(Command::SwitchAccount {
            key: key.clone(),
            account_display_name: "Cacct".to_owned(),
            launch_settings: SessionLaunchSettings::default(),
        })
        .expect("dispatch switch");

    // Refused on turn_pending alone: still on account A, still live.
    assert_eq!(
        workspace.config_dir_for(&key),
        Some(PathBuf::from("/tmp/forge-switch-a")),
        "the just-committed turn is NOT torn down",
    );
    assert!(workspace.has_agent_for(&key), "the session keeps its live agent");

    let mut saw_notice = false;
    while let Ok(update) = updates.try_recv() {
        if let SessionUpdate::SlashCommandError { message, .. } = update
            && message.contains("Finish or cancel")
        {
            saw_notice = true;
        }
    }
    assert!(saw_notice, "the racy switch surfaces the idle notice");
}
