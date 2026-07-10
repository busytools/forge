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
use std::sync::{Arc, Weak};
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

/// Hard cap on loading-loop iterations before the task gives up and
/// bails the account. Without the cap, a refresh that succeeds but
/// yields a token that still 401s on probe would spin the loop
/// forever, burning quota on every cycle. 12 iterations at the
/// 2 s default sleep is ~24 s upper bound, which is generous for
/// recovery from transient errors but bounded against infinite
/// thrash. Each retry budget also counts against the spawn cost
/// of `refresh_via_cli_spawn` (a billed API call), so the cap is
/// also a runaway-cost guard.
const MAX_LOADING_ITERATIONS: u32 = 12;

/// 30 s polling interval for the recovery loop.
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Per-call timeout for `claude auth status` invocations from the
/// recovery poll. The shellout is normally ~50 ms (see auth_status.rs);
/// 5 s is a generous upper bound that absorbs a slow keychain prompt
/// or a network-mounted home dir without holding up the rest of the
/// recovery cycle for an unresponsive process. On timeout the
/// account stays Bailed and the next 30 s tick retries.
const RECOVERY_AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// What the boot-loading loop should do with a probe error, decided
/// from the plan + the error. Keeps the loader's branching testable and
/// its error classification unified with the poller (both route the
/// status through [`crate::workspace::classify_oauth_usage_error`]).
#[derive(Debug, PartialEq)]
enum BootProbeAction {
    /// Keychain auth failure: fire the CLI-spawn refresh recovery. Never
    /// chosen for a base-url account - its probe doesn't read a keychain
    /// token, so a refresh would burn billed `claude -p hi` spawns for
    /// nothing.
    Refresh,
    /// Terminal failure: record the status (an auth error bails the
    /// account), recompute the plan, and stop the task. A base-url auth
    /// failure lands here; the 60 s usage poller re-probes it later and
    /// flips it Ready when the endpoint heals.
    Terminal(UsageFetchStatus),
    /// Transient failure: record the status, back off, and loop. Carries
    /// the server `Retry-After` for a 429.
    RetryLoop(UsageFetchStatus, Option<Duration>),
}

/// Decide the [`BootProbeAction`] for a boot-probe error. Auth failures
/// refresh on the keychain path but are terminal on the base-url path;
/// a 429 or any network / HTTP / decode failure is a retry-loop. The
/// status is the shared [`crate::workspace::classify_oauth_usage_error`]
/// verdict so the loader and poller never label the same error
/// differently.
fn boot_probe_action(is_base_url: bool, err: &OauthUsageError) -> BootProbeAction {
    let status = crate::workspace::classify_oauth_usage_error(err);
    match err {
        OauthUsageError::NoCredentials
        | OauthUsageError::Expired
        | OauthUsageError::Unauthorized(_) => {
            if is_base_url {
                BootProbeAction::Terminal(status)
            } else {
                BootProbeAction::Refresh
            }
        }
        OauthUsageError::RateLimited { retry_after } => {
            BootProbeAction::RetryLoop(status, *retry_after)
        }
        OauthUsageError::Network(_)
        | OauthUsageError::HttpStatus(_, _)
        | OauthUsageError::Decode(_) => BootProbeAction::RetryLoop(status, None),
    }
}

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
                workspace.account_states().lock().set_loading(&account_key, LoadingState::Bailed);
                workspace.recompute_plan_if_ready();
            }
            return;
        }
        let Some(workspace) = workspace_weak.upgrade() else {
            // Workspace dropped during shutdown; exit cleanly.
            return;
        };
        // One decision (probe_plan) drives the probe source, the
        // response-mapping strictness, and whether a 401 is eligible for
        // the keychain refresh. A base-url-override account
        // (`ANTHROPIC_BASE_URL` in `[accounts.env]`) probes its own
        // endpoint with the env bearer and skips the keychain; a normal
        // account uses the keychain + default host.
        let account_env =
            workspace.account_states().lock().env(&account_key).cloned().unwrap_or_default();
        let plan = oauth_usage::probe_plan(&account_env);
        let is_base_url = matches!(plan, oauth_usage::ProbePlan::BaseUrl { .. });
        let probe_result = match &plan {
            oauth_usage::ProbePlan::BaseUrl { base_url, bearer } => {
                let creds = oauth_credentials::OauthCredentials {
                    access_token: bearer.clone(),
                    expires_at: None,
                };
                oauth_usage::probe(&creds, Some(base_url)).await
            }
            oauth_usage::ProbePlan::Keychain => {
                match oauth_credentials::load_oauth_credentials(&config_dir) {
                    Some(creds) => oauth_usage::probe(&creds, None).await,
                    None => Err(OauthUsageError::NoCredentials),
                }
            }
        };

        match probe_result {
            Ok(payload) => {
                // Map per plan (shared with the poller): base-url leniently
                // (each window independent, cold `{}` -> n/a), keychain
                // strictly (a 200 must carry five_hour). A keychain mapping
                // failure is a transient response-shape drift; back off +
                // retry. Base-url never errors here.
                let snapshot = match oauth::map_probe_snapshot(is_base_url, payload) {
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
            Err(err) => match boot_probe_action(is_base_url, &err) {
                BootProbeAction::Refresh => {
                    // Keychain auth-recovery (never base-url). Transition to
                    // Refreshing so the launchpad shows in-flight; fire the
                    // CLI-spawn refresh (pre-gated via auth_status). On
                    // success loop + re-probe; any failure Bails and the
                    // 30 s recovery poll retries once auth_status flips back.
                    workspace
                        .account_states()
                        .lock()
                        .set_loading(&account_key, LoadingState::Refreshing);
                    match oauth_credentials::refresh_via_cli_spawn(&config_dir).await {
                        Ok(_new_creds) => {
                            workspace
                                .account_states()
                                .lock()
                                .set_loading(&account_key, LoadingState::Loading);
                        }
                        Err(refresh_err) => {
                            tracing::warn!(
                                target: "forge_workspace::account_loader",
                                account = %account_key.0,
                                error = %refresh_err,
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
                BootProbeAction::Terminal(status) => {
                    // A base-url auth failure the keychain refresh can't
                    // help. Record it (Unauthorized/Expired bail + stay
                    // visible), recompute the plan, and RETURN so the task
                    // doesn't spin the iteration cap or hold lead assignment
                    // stale. The 60 s usage poller re-probes the account and
                    // flips it Ready once the endpoint heals.
                    tracing::warn!(
                        target: "forge_workspace::account_loader",
                        account = %account_key.0,
                        error = %err,
                        status = ?status,
                        "boot probe hit a terminal error; account Bailed",
                    );
                    workspace.account_states().lock().set_last_error(&account_key, status, None);
                    workspace.recompute_plan_if_ready();
                    return;
                }
                BootProbeAction::RetryLoop(status, retry_after) => {
                    // Transient (network / rate-limit): record + back off +
                    // loop. A 429 carries the server Retry-After.
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
                    tokio::time::sleep(retry_after.unwrap_or(PROBE_RETRY_INTERVAL)).await;
                }
            },
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
pub async fn run_recovery_poll(workspace_weak: Weak<Workspace>) {
    let mut tick = tokio::time::interval(RECOVERY_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Drain the immediate-fire tick - the boot-time loading tasks
    // are still running when this poll starts; first useful tick is
    // one interval out.
    tick.tick().await;
    loop {
        tick.tick().await;
        let Some(workspace) = workspace_weak.upgrade() else {
            // Workspace dropped during shutdown; exit cleanly.
            return;
        };

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

        // Fan out the auth_status shellouts concurrently via JoinSet
        // so N bailed accounts don't add their per-call latency
        // sequentially. Each call wraps in a 5s timeout - a wedged
        // claude process (rare but observed during network-mounted
        // home dir hiccups) shouldn't block the rest of the cycle.
        let mut join_set = tokio::task::JoinSet::new();
        for (key, config_dir) in bailed {
            let dir = config_dir.clone();
            let key_for_task = key.clone();
            join_set.spawn(async move {
                let outcome = tokio::time::timeout(
                    RECOVERY_AUTH_STATUS_TIMEOUT,
                    tokio::task::spawn_blocking(move || {
                        auth_status::account_info_from_shell(&dir).is_some()
                    }),
                )
                .await;
                let logged_in = match outcome {
                    Ok(Ok(b)) => b,
                    Ok(Err(join_err)) => {
                        tracing::warn!(
                            target: "forge_workspace::account_loader",
                            account = %key_for_task.0,
                            error = %join_err,
                            "recovery poll auth_status spawn_blocking panicked; treating as not-logged-in",
                        );
                        false
                    }
                    Err(_) => {
                        tracing::warn!(
                            target: "forge_workspace::account_loader",
                            account = %key_for_task.0,
                            timeout_secs = RECOVERY_AUTH_STATUS_TIMEOUT.as_secs(),
                            "recovery poll auth_status timed out; account stays Bailed for this cycle",
                        );
                        false
                    }
                };
                (key_for_task, config_dir, logged_in)
            });
        }

        while let Some(result) = join_set.join_next().await {
            let Ok((key, config_dir, logged_in)) = result else {
                // join_set itself errored. Tokio JoinError covers
                // task panic + task cancellation; either way we
                // can't distinguish the account that failed at this
                // point, so log without a key.
                tracing::warn!(
                    target: "forge_workspace::account_loader",
                    event_name = "recovery_poll_join_error",
                    "recovery-poll JoinSet returned an error; account auth_status result lost for this cycle",
                );
                continue;
            };
            if !logged_in {
                // Differentiates the kept-Bailed cycle from the
                // pre-recovery cycle in logs. Auth_status's own
                // warn logs cover WHY (binary missing, exit
                // non-zero, malformed JSON, or loggedIn=false);
                // this trace closes the decision trail at the
                // recovery-poll layer so a triage can see "yes,
                // we checked auth_status and decided not to
                // recover" rather than guessing why the account
                // stayed Bailed.
                tracing::trace!(
                    target: "forge_workspace::account_loader",
                    event_name = "recovery_poll_kept_bailed",
                    account = %key.0,
                    "auth_status did not report logged-in; account stays Bailed for this cycle",
                );
                continue;
            }

            // Transition Bailed -> Loading + re-spawn the loading
            // task. The loading task runs until terminal; on Ready
            // it calls recompute_plan_if_ready which (via the
            // frozen overlay added in Section 4.4) extends the plan
            // with the newly-recovered account while preserving
            // existing assignments. Pass Weak<Workspace> so the
            // re-spawn task also auto-exits on shutdown.
            workspace.account_states().lock().set_loading(&key, LoadingState::Loading);
            let weak_clone = Arc::downgrade(&workspace);
            let span = tracing::info_span!("account_recovery_loading", account = %key.0);
            tokio::spawn(
                async move {
                    run_account_loading(config_dir, key, weak_clone).await;
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

    #[test]
    fn boot_probe_action_base_url_auth_is_terminal_not_refresh() {
        // The base-url path never spawns the keychain refresh; an auth
        // failure is terminal (visible + bailed), NOT Refresh, NOT a loop.
        assert_eq!(
            boot_probe_action(true, &OauthUsageError::Unauthorized(401)),
            BootProbeAction::Terminal(UsageFetchStatus::Unauthorized),
        );
    }

    #[test]
    fn boot_probe_action_keychain_auth_refreshes() {
        for err in [
            OauthUsageError::Unauthorized(401),
            OauthUsageError::Expired,
            OauthUsageError::NoCredentials,
        ] {
            assert_eq!(boot_probe_action(false, &err), BootProbeAction::Refresh, "{err:?}");
        }
    }

    #[test]
    fn boot_probe_action_network_is_retry_loop_on_either_plan() {
        for is_base_url in [true, false] {
            assert_eq!(
                boot_probe_action(is_base_url, &OauthUsageError::Network("dns".to_owned())),
                BootProbeAction::RetryLoop(UsageFetchStatus::NetworkFailed, None),
            );
        }
    }

    #[test]
    fn boot_probe_action_rate_limited_carries_retry_after() {
        let retry_after = Some(Duration::from_secs(60));
        assert_eq!(
            boot_probe_action(true, &OauthUsageError::RateLimited { retry_after }),
            BootProbeAction::RetryLoop(UsageFetchStatus::RateLimited, retry_after),
        );
    }
}
