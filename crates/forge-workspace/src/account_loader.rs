//! Per-account boot-time loading task.
//!
//! Drives one account through a state machine until it reaches a
//! terminal `LoadingState` (`Ready` or `Bailed`). Spawned at
//! `Workspace::new`, one tokio task per `[[accounts]]` entry in
//! forge.toml. The launchpad blocks project-row clicks until
//! `AccountStateMap::all_loaded()` returns true (every account in a
//! terminal state); the assignment-plan computation (Section 2.4)
//! only includes accounts whose terminal state is `Ready`.
//!
//! Outline of one iteration:
//! 1. Read keychain via `oauth_credentials::load_oauth_credentials`.
//! 2. Probe `/api/oauth/usage` via `oauth_usage::probe`.
//! 3. Branch on the probe result:
//!    - 200 -> snapshot stored via `set_usage`, transitions to
//!      `Ready`, task exits.
//!    - `NoCredentials` / `Expired` / `Unauthorized` -> auth-recovery
//!      path. Transition to `Refreshing`, fire
//!      `refresh_via_cli_spawn`. On success, transition back to
//!      `Loading` and loop. On failure (NotLoggedIn or any other
//!      `RefreshError`), transition to `Bailed`, task exits.
//!    - `RateLimited` / `HttpStatus` / `Network` / `Decode` ->
//!      transient probe failure. Sleep `PROBE_RETRY_INTERVAL`
//!      (or the server-provided `retry_after`, when present), loop.
//!      Loading state stays in `Loading`; the account remains
//!      dimmed on the launchpad until something resolves.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::Instrument;

use forge_agent::cloud::{auth_status, oauth, oauth_credentials, oauth_usage};
use forge_primitives::usage::oauth::OauthUsageError;

use crate::account::{AccountKey, LoadingState, UsageFetchStatus};
use crate::workspace::Workspace;

/// Sleep duration between transient-error retries when the server
/// didn't provide an explicit `Retry-After`. Short enough that a
/// passing network glitch resolves within a few seconds; long enough
/// that we don't burn CPU through a sustained outage.
const PROBE_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// 30 s polling interval for the recovery loop.
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Run the boot-time loading state machine for one account until it
/// reaches a terminal `LoadingState`.
///
/// Takes `Arc<Workspace>` so the recovery poll (Section 4.3 of #246)
/// can reuse the same entry point - both call sites need access to
/// the workspace's account map + (later) the assignment-plan
/// recompute trigger. The task acquires the parking_lot lock only
/// for the duration of single mutator calls, never across an
/// `await`, so it doesn't block other workspace operations.
pub async fn run_account_loading(
    config_dir: PathBuf,
    account_key: AccountKey,
    workspace: Arc<Workspace>,
) {
    loop {
        let probe_result = match oauth_credentials::load_oauth_credentials(&config_dir) {
            Some(creds) => oauth_usage::probe(&creds).await,
            None => Err(OauthUsageError::NoCredentials),
        };

        match probe_result {
            Ok(payload) => {
                let snapshot = match oauth::snapshot_from_payload(payload) {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!(
                            target: "forge_workspace::account_loader",
                            account = %account_key.0,
                            error = ?err,
                            "boot probe returned 200 but snapshot mapping failed; retrying",
                        );
                        tokio::time::sleep(PROBE_RETRY_INTERVAL).await;
                        continue;
                    }
                };
                workspace.account_states().lock().set_usage(&account_key, snapshot);
                workspace.recompute_plan_if_ready();
                tracing::info!(
                    target: "forge_workspace::account_loader",
                    account = %account_key.0,
                    "boot loading task reached Ready",
                );
                return;
            }
            Err(
                OauthUsageError::NoCredentials
                | OauthUsageError::Expired
                | OauthUsageError::Unauthorized(_),
            ) => {
                // Auth-recovery path. Transition to Refreshing so the
                // launchpad shows in-flight; fire the CLI-spawn refresh
                // which internally pre-gates via auth_status. Any
                // failure (including NotLoggedIn) transitions to
                // Bailed - the 30 s recovery poll picks the account
                // back up when auth_status flips back.
                workspace
                    .account_states()
                    .lock()
                    .set_loading(&account_key, LoadingState::Refreshing);
                match oauth_credentials::refresh_via_cli_spawn(&config_dir).await {
                    Ok(_new_creds) => {
                        // Fresh creds landed; loop and re-probe. Reset
                        // to Loading so the launchpad glyph reflects
                        // the next probe attempt rather than the
                        // mid-refresh state.
                        workspace
                            .account_states()
                            .lock()
                            .set_loading(&account_key, LoadingState::Loading);
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "forge_workspace::account_loader",
                            account = %account_key.0,
                            error = %err,
                            "refresh_via_cli_spawn failed during boot loading; account Bailed",
                        );
                        workspace
                            .account_states()
                            .lock()
                            .set_loading(&account_key, LoadingState::Bailed);
                        workspace.recompute_plan_if_ready();
                        return;
                    }
                }
            }
            Err(OauthUsageError::RateLimited { retry_after }) => {
                workspace.account_states().lock().set_last_error(
                    &account_key,
                    UsageFetchStatus::RateLimited,
                    retry_after,
                );
                tokio::time::sleep(retry_after.unwrap_or(PROBE_RETRY_INTERVAL)).await;
            }
            Err(err) => {
                tracing::debug!(
                    target: "forge_workspace::account_loader",
                    account = %account_key.0,
                    error = %err,
                    "boot probe returned transient error; retrying",
                );
                workspace.account_states().lock().set_last_error(
                    &account_key,
                    UsageFetchStatus::NetworkFailed,
                    None,
                );
                tokio::time::sleep(PROBE_RETRY_INTERVAL).await;
            }
        }
    }
}

/// Background poll that watches Bailed accounts and re-runs the
/// boot-time loading flow whenever `claude auth status` reports the
/// account is logged-in again. Ticks every
/// [`RECOVERY_POLL_INTERVAL`]; one tokio task for the workspace
/// lifetime, spawned at `Workspace::start_account_loading_tasks`.
///
/// The auth_status shell-out runs under `spawn_blocking` (it's a
/// synchronous std::process::Command shellout from
/// `forge_agent::cloud::auth_status`) so it doesn't stall the tokio
/// reactor. For each Bailed account whose `auth_status` flips back
/// to logged-in, the recovery task transitions the account to
/// `Loading` and spawns a fresh `run_account_loading` task. A
/// subsequent successful probe lands the new snapshot; a subsequent
/// failure flips back to Bailed and the next 30 s tick reevaluates.
pub async fn run_recovery_poll(workspace: Arc<Workspace>) {
    let mut tick = tokio::time::interval(RECOVERY_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Drain the immediate-fire tick - the boot-time loading tasks
    // are still running when this poll starts; first useful tick is
    // one interval out.
    tick.tick().await;
    loop {
        tick.tick().await;

        // Snapshot the bailed accounts under the parking_lot lock,
        // then drop the lock before any awaits.
        let bailed: Vec<(AccountKey, std::path::PathBuf)> = {
            let accounts = workspace.account_states().lock();
            accounts
                .by_key
                .iter()
                .filter(|(_, s)| s.loading == LoadingState::Bailed)
                .map(|(k, s)| (k.clone(), s.config_dir.clone()))
                .collect()
        };

        if bailed.is_empty() {
            continue;
        }

        for (key, config_dir) in bailed {
            let dir = config_dir.clone();
            let logged_in = tokio::task::spawn_blocking(move || {
                auth_status::account_info_from_shell(&dir).is_some()
            })
            .await
            .unwrap_or(false);

            if !logged_in {
                continue;
            }

            // Transition Bailed -> Loading + re-spawn the loading
            // task. The loading task runs until terminal; on Ready
            // it calls recompute_plan_if_ready which (via the
            // frozen overlay added in Section 4.4) extends the plan
            // with the newly-recovered account while preserving
            // existing assignments.
            workspace.account_states().lock().set_loading(&key, LoadingState::Loading);
            let workspace_clone = Arc::clone(&workspace);
            let span = tracing::info_span!("account_recovery_loading", account = %key.0);
            tokio::spawn(
                async move {
                    run_account_loading(config_dir, key, workspace_clone).await;
                }
                .instrument(span),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    // End-to-end behavior tests (probe → branch → refresh + retry)
    // require either a mock claude binary on PATH or a substitution
    // layer over oauth_usage::probe + refresh_via_cli_spawn. Both
    // approaches add significant test infrastructure; the planner
    // approved deferring these to manual smoke against a real
    // expired-token account.
    //
    // What we CAN unit-test here in isolation lives elsewhere: the
    // state-transition primitives on `AccountStateMap` are tested in
    // `account::tests`, and the constants below are pinned for
    // regression. The function itself reads as a flat state machine
    // over those primitives + the existing async probe/refresh
    // entries that PR #240 and PR #243 already cover.

    use super::*;

    #[test]
    fn probe_retry_interval_is_2s() {
        assert_eq!(PROBE_RETRY_INTERVAL, Duration::from_secs(2));
    }
}
