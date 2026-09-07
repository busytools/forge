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
//! 1. The account's provider backend resolves the credential: the
//!    account's `CLAUDE_CODE_OAUTH_TOKEN` when it is token-mode (a
//!    valid token is probed via a minimal billed messages call whose
//!    headers carry the usage windows), or nothing at all.
//! 2. Probe through the backend.
//! 3. Branch on the probe result:
//!    - 200 -> snapshot stored via `set_usage`, transitions to
//!      `Ready`, task exits.
//!    - a 200 whose body maps to nothing -> response-shape drift;
//!      back off and retry.
//!    - `NoCredentials` / `Expired` / `Unauthorized` -> auth failure.
//!      forge has no local credential repair to fire (the env is
//!      boot-frozen), so the account transitions to `Bailed`, task
//!      exits. The 60 s usage poller re-probes the account and flips
//!      it `Ready` once the credential heals.
//!    - `RateLimited` / `HttpStatus` / `Network` / `Decode` ->
//!      transient probe failure. Sleep `PROBE_RETRY_INTERVAL`
//!      (or the server-provided `retry_after`, when present), loop.
//!      Loading state stays in `Loading`; the account remains
//!      dimmed on the launchpad until something resolves.

use std::path::PathBuf;
use std::sync::Weak;
use std::time::Duration;

use forge_providers::{ProbeError, RepairAction};

use crate::account::{AccountKey, LoadingState};
use crate::workspace::Workspace;

/// Sleep duration between transient-error retries when the server
/// didn't provide an explicit `Retry-After`. Short enough that a
/// passing network glitch resolves within a few seconds; long enough
/// that we don't burn CPU through a sustained outage.
const PROBE_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Hard cap on loading-loop iterations before the task gives up and
/// bails the account. Without the cap, a probe that keeps failing
/// transiently would spin the loop forever. 12 iterations at the
/// 2 s default sleep is ~24 s upper bound, which is generous for
/// recovery from transient errors but bounded against infinite
/// thrash.
const MAX_LOADING_ITERATIONS: u32 = 12;

/// Run the boot-time loading state machine for one account until it
/// reaches a terminal `LoadingState`.
///
/// Takes `Weak<Workspace>` so the task auto-exits when the workspace
/// is dropped during shutdown - same pattern `start_usage_poller`
/// uses. Each iteration upgrades the weak; failure to upgrade means
/// the workspace is gone and the task returns. The lock is only
/// acquired for single mutator calls, never across an await, so
/// other workspace operations aren't blocked.
pub async fn run_account_loading(
    config_dir: PathBuf,
    account_key: AccountKey,
    workspace_weak: Weak<Workspace>,
) {
    let mut iteration = 0u32;
    // Whether the previous iteration recorded its own failure class.
    // The retry-loop arm does (it is the class the budget was burned
    // on); the refresh and 200-mapping paths leave any earlier record
    // stale, so the cap must fall back to the unrecorded default
    // rather than bail an auth problem wearing a network label.
    let mut last_iteration_recorded = false;
    loop {
        iteration += 1;
        if iteration > MAX_LOADING_ITERATIONS {
            // Spun the full retry budget without reaching a terminal
            // state. Force-bail so the account doesn't keep hammering
            // refresh + probe forever. Common cause: refresh succeeds
            // but the rotated token still 401s (server-side scope or
            // org change forge can't recover from automatically).
            if let Some(workspace) = workspace_weak.upgrade() {
                tracing::warn!(
                    target: "forge_workspace::account_loader",
                    account = %account_key.0,
                    iterations = MAX_LOADING_ITERATIONS,
                    "loading task hit retry cap without reaching terminal; transitioning to Bailed",
                );
                let mut states = workspace.account_states().lock();
                if !last_iteration_recorded {
                    states.clear_last_error(&account_key);
                }
                states.set_loading(&account_key, LoadingState::Bailed);
                drop(states);
                workspace.recompute_plan_if_ready();
            }
            return;
        }
        let Some(workspace) = workspace_weak.upgrade() else {
            // Workspace dropped during shutdown; exit cleanly.
            return;
        };
        let (provider, account_env) = {
            let accounts = workspace.account_states().lock();
            (
                accounts.provider_or_anthropic(&account_key),
                accounts.env(&account_key).cloned().unwrap_or_default(),
            )
        };
        // The backend owns the probe and the repair verdict; the
        // loader only executes the verdict against its state machine.
        let probe_result =
            crate::provider_probe::probe_via_backend(provider, &config_dir, &account_env).await;

        match probe_result {
            Ok(snapshot) => {
                workspace.account_states().lock().set_usage(&account_key, snapshot);
                workspace.recompute_plan_if_ready();
                tracing::info!(
                    target: "forge_workspace::account_loader",
                    account = %account_key.0,
                    "boot loading task reached Ready",
                );
                return;
            }
            // A 200 whose body maps to nothing is transient response-
            // shape drift; back off + retry. Deliberately handled
            // before the repair verdict: unlike Retry, this arm
            // records no last_error and no iteration-cap record.
            Err(ProbeError::Unmappable(message)) => {
                tracing::warn!(
                    target: "forge_workspace::account_loader",
                    account = %account_key.0,
                    error = %message,
                    "boot probe returned 200 but snapshot mapping failed; retrying",
                );
                last_iteration_recorded = false;
                tokio::time::sleep(PROBE_RETRY_INTERVAL).await;
            }
            Err(err) => {
                // The registry pins every token to a backend, so the
                // fallback is unreachable; it retries like the probe
                // path's missing-registration fabrication.
                let action = crate::provider_probe::backend_for(provider).map_or(
                    RepairAction::Retry { retry_after: None },
                    |backend| {
                        backend.repair(
                            &forge_providers::AccountEnv {
                                config_dir: &config_dir,
                                env: &account_env,
                            },
                            &err,
                        )
                    },
                );
                match action {
                    RepairAction::Terminal => {
                        // An auth failure forge cannot repair (the env
                        // credential is boot-frozen). Record it
                        // (Unauthorized/Expired bail + stay visible),
                        // recompute the plan, and RETURN so the task
                        // doesn't spin the iteration cap or hold lead assignment
                        // stale. The 60 s usage poller re-probes the account and
                        // flips it Ready once the endpoint heals.
                        let status = crate::workspace::classify_oauth_usage_error(&err);
                        tracing::warn!(
                            target: "forge_workspace::account_loader",
                            account = %account_key.0,
                            error = %err,
                            status = ?status,
                            "boot probe hit a terminal error; account Bailed",
                        );
                        workspace.account_states().lock().set_last_error(
                            &account_key,
                            status,
                            None,
                        );
                        workspace.recompute_plan_if_ready();
                        return;
                    }
                    RepairAction::Retry { retry_after } => {
                        // Transient (network / rate-limit): record + back off +
                        // loop. A 429 carries the server Retry-After.
                        let status = crate::workspace::classify_oauth_usage_error(&err);
                        tracing::debug!(
                            target: "forge_workspace::account_loader",
                            account = %account_key.0,
                            error = %err,
                            status = ?status,
                            "boot probe returned transient error; retrying",
                        );
                        workspace.account_states().lock().set_last_error(
                            &account_key,
                            status,
                            retry_after,
                        );
                        last_iteration_recorded = true;
                        tokio::time::sleep(retry_after.unwrap_or(PROBE_RETRY_INTERVAL)).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // End-to-end behavior tests (probe -> branch -> retry) require a
    // substitution layer over the forge-providers probe. The planner
    // approved deferring these to manual smoke against a real
    // expired-token account.
    //
    // What we CAN unit-test here in isolation lives elsewhere: the
    // state-transition primitives on `AccountStateMap` are tested in
    // `account::tests`, the repair table on the backends is pinned in
    // forge-providers, and the constants below are pinned for
    // regression. The function itself reads as a flat state machine
    // over those primitives + the existing async probe entries that
    // PR #240 and PR #243 already cover.

    use super::*;

    #[test]
    fn probe_retry_interval_is_2s() {
        assert_eq!(PROBE_RETRY_INTERVAL, Duration::from_secs(2));
    }
}
