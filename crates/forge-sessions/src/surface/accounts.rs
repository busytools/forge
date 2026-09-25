//! The account pool as a view reads it.

use forge_primitives::account::AccountAuth;
use forge_primitives::usage::UsageSnapshot;
use forge_workspace::{AccountLoadingRow, GatewayOrgView};

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
