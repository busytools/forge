//! `accounts()`: the account pool's state.

use forge_primitives::account::AccountAuth;
use forge_primitives::usage::UsageSnapshot;
use forge_workspace::{AccountLoadingRow, GatewayOrgView, UsageFetchStatus};

use super::ViewSurface;

/// The account pool's state: what the launchpad, preflight and the
/// gateway view render.
pub struct AccountsView {
    /// One row per configured account, in `forge.toml` declaration
    /// order, carrying its loading state and last probe failure.
    pub loading: Vec<AccountLoadingRow>,
    /// `true` when every account has settled and the gateway's listener
    /// is bound.
    pub all_loaded: bool,
    pub gateway: GatewayView,
    /// What each account has left, one row per configured account.
    pub usage: Vec<AccountUsage>,
    /// The gateway's orgs, each with its walk order and the live state of
    /// every account it names.
    pub orgs: Vec<GatewayOrgView>,
}

/// The inference listener's bind state.
pub struct GatewayView {
    pub ready: bool,
    pub port: u16,
    /// Set when the listener could not bind its port.
    pub bind_error: Option<String>,
}

/// One account's cached usage snapshot.
pub struct AccountUsage {
    pub display_name: String,
    /// `None` until the poller first succeeds.
    pub snapshot: Option<UsageSnapshot>,
}

impl AccountsView {
    /// The cached usage snapshot for `display_name`. `None` for an
    /// account the poller has not reached yet, and for a name that is not
    /// configured.
    pub fn usage_for(&self, display_name: &str) -> Option<UsageSnapshot> {
        self.usage.iter().find(|row| row.display_name == display_name)?.snapshot.clone()
    }

    /// How `display_name` authenticates, which the auth-repair hints
    /// branch on. `None` for a name that is not configured.
    pub fn auth_for(&self, display_name: &str) -> Option<AccountAuth> {
        self.loading.iter().find(|row| row.display_name == display_name).map(|row| row.auth)
    }

    /// The most recent poll-attempt failure for `display_name`. `None`
    /// when the last poll succeeded, none has run, or the name is not
    /// configured.
    pub fn usage_error_for(&self, display_name: &str) -> Option<UsageFetchStatus> {
        self.loading.iter().find(|row| row.display_name == display_name)?.last_error
    }
}

impl ViewSurface {
    pub fn accounts(&self) -> AccountsView {
        let workspace = &self.workspace;
        let loading = workspace.account_loading_snapshot();
        // The loading rows are the configured accounts in declaration
        // order, so they name what the usage reads are keyed on.
        let usage = loading
            .iter()
            .map(|row| AccountUsage {
                display_name: row.display_name.clone(),
                snapshot: workspace.usage_for(&row.display_name),
            })
            .collect();
        AccountsView {
            loading,
            all_loaded: workspace.all_accounts_loaded(),
            gateway: GatewayView {
                ready: workspace.gateway_ready(),
                port: workspace.gateway_port(),
                bind_error: workspace.gateway_bind_error(),
            },
            usage,
            orgs: workspace.gateway_view_snapshot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};

    use super::*;

    fn usage(utilization: f64) -> UsageSnapshot {
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
            five_hour: Some(UsageWindow { utilization, resets_at: None, reset_description: None }),
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
            balance: None,
        }
    }

    /// Catches the verb being rebuilt from a different account source, or
    /// a field wired to its neighbour's read, which would render a
    /// different account set than the calls it replaced.
    #[test]
    fn accounts_agrees_with_the_calls_it_replaces() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        workspace.seed_test_usage("Granite", usage(11.0));

        let view = ViewSurface::new(Arc::clone(&workspace)).accounts();

        assert_eq!(
            view.loading,
            workspace.account_loading_snapshot(),
            "the loading rows are the call's own",
        );
        assert_eq!(
            view.all_loaded,
            workspace.all_accounts_loaded(),
            "settling is the same condition",
        );
        assert_eq!(view.gateway.ready, workspace.gateway_ready(), "readiness is the same flag");
        assert_eq!(view.gateway.port, workspace.gateway_port(), "the port is the listener's");
        assert_eq!(
            view.gateway.bind_error,
            workspace.gateway_bind_error(),
            "the bind failure is the same string",
        );
        assert_eq!(
            view.orgs.iter().map(|org| org.org.clone()).collect::<Vec<_>>(),
            workspace.gateway_view_snapshot().iter().map(|org| org.org.clone()).collect::<Vec<_>>(),
            "the gateway view names every org in the same order",
        );

        assert_eq!(
            view.usage_for("Granite"),
            Some(usage(11.0)),
            "the seeded account reports the poller's own snapshot",
        );
        assert_eq!(
            view.auth_for("Granite"),
            Some(AccountAuth::Token),
            "a configured token account resolves rather than falling to the None branch",
        );

        for name in ["Granite", "OpenRouter-TM", "nobody"] {
            assert_eq!(
                view.usage_for(name),
                workspace.usage_for(name),
                "usage_for agrees with the call it replaces for {name}",
            );
            assert_eq!(
                view.usage_error_for(name),
                workspace.usage_error_for(name),
                "usage_error_for agrees with the call it replaces for {name}",
            );
            assert_eq!(
                view.auth_for(name),
                workspace.account_auth_for(name),
                "auth_for agrees with the call it replaces for {name}",
            );
        }
    }
}
