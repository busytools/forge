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

use forge_primitives::usage::UsageSnapshot;
use forge_primitives::usage::oauth::OauthUsageError;
use forge_providers::ProbeError;

use crate::account::{AccountKey, LoadingState};
use crate::workspace::Workspace;

/// Apply one probe outcome to the account state map and return the
/// terminal `LoadingState` it settles to. Every error class is
/// terminal in this single pass; healing is the usage poller's
/// schedule.
fn settle_probe_result(
    states: &mut crate::account::AccountStateMap,
    key: &AccountKey,
    result: &Result<UsageSnapshot, ProbeError>,
) -> LoadingState {
    match result {
        Ok(snapshot) => {
            states.set_usage(key, snapshot.clone());
            LoadingState::Ready
        }
        Err(ProbeError::Unmappable(_)) => {
            // Response-shape drift is not an account fault: records no
            // error; any stale record clears with the bail.
            states.clear_last_error(key);
            states.set_loading(key, LoadingState::Bailed);
            LoadingState::Bailed
        }
        Err(err) => {
            let failure_class = crate::workspace::classify_oauth_usage_error(err);
            let retry_after = match err {
                ProbeError::Fetch(OauthUsageError::RateLimited { retry_after }) => *retry_after,
                _ => None,
            };
            states.set_last_error(key, failure_class, retry_after);
            // set_last_error bails only on the auth classes; the
            // transient ones need the explicit terminal settle.
            states.set_loading(key, LoadingState::Bailed);
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
    let (provider, account_env) = {
        let accounts = workspace.account_states().lock();
        (
            accounts.provider_or_anthropic(&account_key),
            accounts.env(&account_key).cloned().unwrap_or_default(),
        )
    };
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
    {
        let mut states = workspace.account_states().lock();
        settle_probe_result(&mut states, &account_key, &probe_result);
    }
    workspace.recompute_plan_if_ready();
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::account::{AccountStateMap, UsageFetchStatus};
    use forge_primitives::usage::oauth::OauthUsageError;
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
    use forge_providers::ProbeError;
    use std::time::{Duration, Instant};

    // Mirrors the `make_account` helper idiom in `account::tests`.
    fn states_with_one_account() -> AccountStateMap {
        AccountStateMap::new(&[crate::config::LoadedAccount {
            display_name: "test".to_owned(),
            config_dir: std::path::PathBuf::from("/fake/test"),
            provider: forge_primitives::account::Provider::Anthropic,
            env: std::collections::HashMap::new(),
            experimental: false,
            permission_mode: None,
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
        let mut states = states_with_one_account();
        let key = AccountKey("test".to_owned());
        let state = settle_probe_result(
            &mut states,
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
            states.usage_error(&key),
            Some(UsageFetchStatus::RateLimited),
            "the rate-limit class is recorded for the row's reason text",
        );
        // The server Retry-After schedules the re-probe; the local
        // exponential default would be 30 s, so an hour-long gap
        // proves the server value was honoured, not discarded.
        let gap = states
            .by_key
            .get(&key)
            .and_then(|s| s.next_probe_at)
            .map(|t| t.saturating_duration_since(Instant::now()));
        assert!(
            gap.is_some_and(|g| g > Duration::from_secs(3599)),
            "Retry-After retained in the probe schedule; got {gap:?}",
        );
    }

    #[test]
    fn settle_probe_result_bails_unauthorized_with_its_status() {
        let mut states = states_with_one_account();
        let key = AccountKey("test".to_owned());
        let state = settle_probe_result(
            &mut states,
            &key,
            &Err(ProbeError::Fetch(OauthUsageError::Unauthorized(401))),
        );
        assert_eq!(state, LoadingState::Bailed);
        assert_eq!(
            states.loading_state(&key),
            LoadingState::Bailed,
            "the terminal settle is stored, not just returned",
        );
        assert_eq!(states.usage_error(&key), Some(UsageFetchStatus::Unauthorized),);
    }

    #[test]
    fn settle_probe_result_bails_unmappable_without_an_error_record() {
        let mut states = states_with_one_account();
        let key = AccountKey("test".to_owned());
        let state = settle_probe_result(
            &mut states,
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
            states.usage_error(&key).is_none(),
            "Unmappable records no error, preserving today's nuance",
        );
    }

    #[test]
    fn settle_probe_result_settles_successful_probe_ready() {
        let mut states = states_with_one_account();
        let key = AccountKey("test".to_owned());
        let state = settle_probe_result(&mut states, &key, &Ok(snapshot()));
        assert_eq!(state, LoadingState::Ready);
        assert_eq!(
            states.loading_state(&key),
            LoadingState::Ready,
            "the terminal settle is stored, not just returned",
        );
        assert!(states.usage(&key).is_some(), "the snapshot is cached");
    }
}
