//! Per-account boot-time loading task.
//!
//! Probes one account once and settles it to a terminal `LoadingState`
//! (`Ready` or `Bailed`) in that single round. Spawned at
//! `Workspace::new`, one tokio task per `[[accounts]]` entry in
//! forge.toml. The launchpad blocks project-row clicks until
//! `AccountStateMap::all_loaded()` returns true (every account in a
//! terminal state); the assignment-plan computation (Section 2.4)
//! only includes accounts whose terminal state is `Ready`.
//!
//! One probe per task, every outcome terminal:
//! - 200 -> snapshot stored via `set_usage`, `Ready`, task exits.
//! - Any probe error -> `Bailed` in the same round. Auth failures
//!   record their status; a 200 whose body maps to nothing
//!   (`Unmappable`) records nothing; the transient classes
//!   (`RateLimited` etc.) record status + the server `Retry-After`,
//!   which also schedules the poller's re-probe.
//!
//! Healing is the pollers' job: the 60 s usage poller re-probes
//! accounts once their `Retry-After` / backoff window passes, flips
//! `Bailed` -> `Ready`, and recomputes the assignment plan so the
//! recovered account rejoins its pools without shifting running
//! sessions. A rate-limited probe does not mean inference is
//! limited - the session surfaces its own error if it is.

use std::sync::Weak;

use forge_gateway::ProbeError;
use forge_primitives::usage::UsageSnapshot;
use forge_primitives::usage::oauth::OauthUsageError;

use forge_gateway::{AccountKey, AccountPool, LoadingState};

use crate::workspace::Workspace;

/// Apply one probe outcome to the account state map and return the
/// terminal `LoadingState` it settles to. Every error class is
/// terminal in this single pass; healing is the usage poller's
/// schedule.
fn settle_probe_result(
    pool: &AccountPool,
    key: &AccountKey,
    result: &Result<UsageSnapshot, ProbeError>,
) -> LoadingState {
    match result {
        Ok(snapshot) => {
            pool.set_usage(key, snapshot.clone());
            LoadingState::Ready
        }
        Err(ProbeError::Unmappable(_)) => {
            // Response-shape drift is not an account fault: records no
            // error; any stale record clears with the bail.
            pool.clear_last_error(key);
            pool.set_loading(key, LoadingState::Bailed);
            LoadingState::Bailed
        }
        Err(err) => {
            let failure_class = crate::workspace::classify_oauth_usage_error(err);
            let retry_after = match err {
                ProbeError::Fetch(OauthUsageError::RateLimited { retry_after }) => *retry_after,
                _ => None,
            };
            pool.set_last_error(key, failure_class, retry_after);
            // set_last_error bails only on the auth classes; the
            // transient ones need the explicit terminal settle.
            pool.set_loading(key, LoadingState::Bailed);
            LoadingState::Bailed
        }
    }
}

/// Probe the account once and settle its terminal `LoadingState`.
///
/// Takes `Weak<Workspace>` so the task auto-exits when the workspace
/// is dropped during shutdown - same pattern `start_usage_poller`
/// uses. The lock is only acquired for single mutator calls, never
/// across an await, so other workspace operations aren't blocked.
pub async fn run_account_loading(account_key: AccountKey, workspace_weak: Weak<Workspace>) {
    let Some(workspace) = workspace_weak.upgrade() else {
        // Workspace dropped during shutdown; exit cleanly.
        return;
    };
    let pool = workspace.account_pool();
    let Some(provider) = pool.provider(&account_key) else {
        tracing::warn!(
            target: "forge_workspace::account_loader",
            account = %account_key.0,
            "no provider for the account key; skipping its boot probe",
        );
        return;
    };
    let account_env = pool.env(&account_key).unwrap_or_default();
    // The backend owns the probe; the settle helper owns the verdict
    // against the state machine.
    let probe_result = crate::provider_probe::probe_via_backend(provider, &account_env).await;
    match &probe_result {
        Ok(_) => tracing::info!(
            target: "forge_workspace::account_loader",
            account = %account_key.0,
            "boot probe reached Ready",
        ),
        Err(err) => tracing::warn!(
            target: "forge_workspace::account_loader",
            account = %account_key.0,
            error = %err,
            "boot probe failed; account settled Bailed; the usage poller will re-probe it",
        ),
    }
    settle_probe_result(pool, &account_key, &probe_result);
    workspace.recompute_plan_if_ready();
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use forge_gateway::ProbeError;
    use forge_gateway::UsageFetchStatus;
    use forge_primitives::usage::oauth::OauthUsageError;
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
    use std::time::Duration;

    // Mirrors the `make_account` helper idiom in `account::tests`.
    fn states_with_one_account() -> AccountPool {
        AccountPool::new(&[crate::config::LoadedAccount {
            display_name: "test".to_owned(),
            provider: forge_primitives::account::Provider::Anthropic,
            base_url: None,
            models: vec!["claude-sonnet-5".to_owned()],
            model_aliases: std::collections::HashMap::new(),
            model_slugs: std::collections::HashMap::new(),
            env: std::collections::HashMap::new(),
        }])
    }

    fn snapshot() -> UsageSnapshot {
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
            five_hour: Some(UsageWindow {
                utilization: 10.0,
                resets_at: Some(std::time::SystemTime::now() + Duration::from_secs(3600)),
                reset_description: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: 20.0,
                resets_at: Some(std::time::SystemTime::now() + Duration::from_secs(3600)),
                reset_description: None,
            }),
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
            balance: None,
        }
    }

    #[test]
    fn settle_probe_result_bails_rate_limited_with_status_and_retry_after() {
        let states = states_with_one_account();
        let key = AccountKey("test".to_owned());
        let state = settle_probe_result(
            &states,
            &key,
            &Err(ProbeError::Fetch(OauthUsageError::RateLimited {
                retry_after: Some(Duration::from_secs(3600)),
            })),
        );
        assert_eq!(state, LoadingState::Bailed);
        assert_eq!(
            states.loading_state(&key),
            LoadingState::Bailed,
            "the terminal settle is stored, not just returned",
        );
        assert_eq!(
            states.usage_error("test"),
            Some(UsageFetchStatus::RateLimited),
            "the rate-limit class is recorded for the row's reason text",
        );
        // The server Retry-After schedules the re-probe (clamped to
        // the 10-minute ceiling); the local exponential default would
        // be 30 s, so a ten-minute gap proves the server value was
        // honoured, not discarded.
        let gap = states.next_probe_after(&key);
        assert!(
            gap.is_some_and(|g| g > Duration::from_secs(599) && g <= Duration::from_secs(601)),
            "Retry-After retained (clamped) in the probe schedule; got {gap:?}",
        );
    }

    #[test]
    fn settle_probe_result_bails_unauthorized_with_its_status() {
        let states = states_with_one_account();
        let key = AccountKey("test".to_owned());
        let state = settle_probe_result(
            &states,
            &key,
            &Err(ProbeError::Fetch(OauthUsageError::Unauthorized(401))),
        );
        assert_eq!(state, LoadingState::Bailed);
        assert_eq!(
            states.loading_state(&key),
            LoadingState::Bailed,
            "the terminal settle is stored, not just returned",
        );
        assert_eq!(states.usage_error("test"), Some(UsageFetchStatus::Unauthorized),);
    }

    #[test]
    fn settle_probe_result_bails_unmappable_without_an_error_record() {
        let states = states_with_one_account();
        let key = AccountKey("test".to_owned());
        // A stale record from an earlier probe must not survive the
        // shape-drift bail - the bail records nothing and clears.
        states.set_last_error(&key, UsageFetchStatus::RateLimited, None);
        let state = settle_probe_result(
            &states,
            &key,
            &Err(ProbeError::Unmappable("shape drift".to_owned())),
        );
        assert_eq!(state, LoadingState::Bailed);
        assert_eq!(
            states.loading_state(&key),
            LoadingState::Bailed,
            "the terminal settle is stored, not just returned",
        );
        assert!(
            states.usage_error("test").is_none(),
            "Unmappable records no error, preserving today's nuance",
        );
    }

    #[test]
    fn settle_probe_result_settles_successful_probe_ready() {
        let states = states_with_one_account();
        let key = AccountKey("test".to_owned());
        let state = settle_probe_result(&states, &key, &Ok(snapshot()));
        assert_eq!(state, LoadingState::Ready);
        assert_eq!(
            states.loading_state(&key),
            LoadingState::Ready,
            "the terminal settle is stored, not just returned",
        );
        assert!(states.usage("test").is_some(), "the snapshot is cached");
    }
}
