//! Slack and Gotify subscription state as a view reads it.

use std::collections::BTreeMap;

use forge_primitives::GotifySubscription;
use forge_primitives::slack::SlackSubscription;

use super::ViewSurface;

/// What the inbound connectors are watching.
pub struct ConnectorsView {
    pub gotify: GotifyView,
    pub slack: SlackView,
}

/// The Gotify stream's liveness and one project's subscriptions.
pub struct GotifyView {
    pub connected: bool,
    pub subscriptions: Vec<GotifySubscription>,
}

/// Slack's per-workspace pump liveness and one project's subscriptions.
pub struct SlackView {
    /// Workspace label to whether that workspace's pump is live.
    pub connected_workspaces: BTreeMap<String, bool>,
    /// `true` when the stored subscription set failed to load.
    pub load_failed: bool,
    pub subscriptions: Vec<SlackSubscription>,
}

impl ViewSurface {
    /// `project` scopes the subscription lists to that project's own.
    /// `None` reports only the connection status, the way an unstamped
    /// session reads.
    pub fn connectors(&self, project: Option<&str>) -> ConnectorsView {
        let workspace = &self.workspace;
        ConnectorsView {
            gotify: GotifyView {
                connected: workspace.gotify_connected(),
                subscriptions: project
                    .map(|name| workspace.gotify_subscriptions_for_project(name))
                    .unwrap_or_default(),
            },
            slack: SlackView {
                connected_workspaces: workspace.slack_connected_workspaces(),
                load_failed: workspace.slack_subscription_load_failed(),
                subscriptions: project
                    .map(|name| workspace.slack_subscriptions_for_project(name))
                    .unwrap_or_default(),
            },
        }
    }
}
