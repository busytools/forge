//! `/account` mid-session switch integration tests. Verify that
//! `Command::SwitchAccount` re-spawns the SAME session key under the
//! picked account's `config_dir` (forcing the account, not letting the
//! round-robin picker choose), and that the server-side backstop
//! refuses a switch while a turn is in flight. Mirror `workspace_handles`:
//! assert at the `AgentHandle`/bridge `config_dir` boundary, no real
//! `claude` conversation required.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
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
token = "ta"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Bacct"
token = "tb"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Cacct"
token = "tc"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
    )
    .expect("write forge.toml");
    Arc::new(Workspace::new_for_test(dir.to_owned()).expect("new"))
}

#[tokio::test]
async fn switch_account_respawns_same_session_under_forced_account() {
    // Three accounts. A cold-cache initial spawn takes cursor=0 (Aacct);
    // an UNforced re-spawn would advance the round-robin to cursor=1
    // (Bacct). The switch targets Cacct, so landing on Cacct's
    // credential proves the account was FORCED, not merely rotated to.
    // One shared config dir means the dir no longer distinguishes
    // accounts - the stamped credential does.
    let dir = tempdir().expect("tempdir");
    let workspace = three_account_workspace(dir.path());

    // Initial spawn: cold cache -> first usable in the pin (Aacct).
    let key = SessionKey::from_str_for_test("switch-target");
    let handle = workspace
        .get_agent_handle(SessionTarget::Session(key.clone()), SessionLaunchSettings::default())
        .expect("initial spawn");
    let credential = |handle: &std::sync::Arc<forge_agent::AgentHandle>| {
        handle.env().get("CLAUDE_CODE_OAUTH_TOKEN").map(std::string::ToString::to_string)
    };
    assert_eq!(
        credential(&handle).as_deref(),
        Some("ta"),
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

    // Same key, re-spawned under the FORCED account C. Re-resolve the
    // handle by key: the switch replaced the pooled agent.
    let respawned = workspace
        .get_agent_handle(SessionTarget::Session(key.clone()), SessionLaunchSettings::default())
        .expect("pooled handle");
    assert_eq!(
        credential(&respawned).as_deref(),
        Some("tc"),
        "switch re-spawns the SAME session key under the forced account C",
    );
    assert!(
        workspace.has_agent_for(&key),
        "the re-spawned session keeps its live agent under the same key",
    );
}

#[tokio::test]
async fn switch_account_refused_while_a_turn_is_in_flight() {
    // The authoritative backstop: a delivered peer / cron / gotify / slack
    // prompt can start a turn between picker-open and Enter. handle_switch_account
    // must refuse (notice, no teardown) rather than tear down the live turn.
    let dir = tempdir().expect("tempdir");
    let workspace = three_account_workspace(dir.path());
    let mut updates = workspace.subscribe().expect("subscribe");

    let key = SessionKey::from_str_for_test("switch-busy");
    workspace
        .get_agent_handle(SessionTarget::Session(key.clone()), SessionLaunchSettings::default())
        .expect("initial spawn");
    assert!(workspace.has_agent_for(&key), "fixture premise: the session is live");

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
    assert!(workspace.has_agent_for(&key), "a busy session is NOT switched",);

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
    assert!(workspace.has_agent_for(&key), "the just-committed turn is NOT torn down");

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
