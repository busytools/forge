//! `connectors()`: the Slack and Gotify subscription state.

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Catches a field wired to the sibling connector's read, or the
    /// project scope being dropped, which would show one connector's
    /// state under the other's heading.
    ///
    /// The two liveness flags are seeded to opposite values on purpose:
    /// two false-valued bools read as agreement, which is what let a
    /// read wired to the wrong connector pass unnoticed.
    #[test]
    fn connectors_agrees_with_the_calls_it_replaces() {
        let (workspace, _dir) = crate::surface::testing::workspace_with_connector_subs();
        workspace.seed_test_gotify_connected(true);

        let view = ViewSurface::new(Arc::clone(&workspace)).connectors(Some("forge"));

        assert_ne!(
            workspace.gotify_connected(),
            workspace.slack_subscription_load_failed(),
            "the fixture gives the two connectors' flags different values, or the comparisons below agree on a false",
        );
        assert_ne!(
            view.gotify.connected, view.slack.load_failed,
            "gotify's liveness and slack's load failure are read from different flags, not one value under both headings",
        );
        assert_eq!(
            view.gotify.connected,
            workspace.gotify_connected(),
            "gotify's liveness is the same flag",
        );
        assert_eq!(
            view.gotify.subscriptions,
            workspace.gotify_subscriptions_for_project("forge"),
            "gotify's rows are the project's own",
        );
        assert_eq!(
            view.slack.connected_workspaces,
            workspace.slack_connected_workspaces(),
            "slack's per-workspace liveness is the same map",
        );
        assert_eq!(
            view.slack.load_failed,
            workspace.slack_subscription_load_failed(),
            "slack's load failure is the same flag",
        );
        assert_eq!(
            view.slack.subscriptions,
            workspace.slack_subscriptions_for_project("forge"),
            "slack's rows are the project's own",
        );
        assert_eq!(
            (view.gotify.subscriptions.len(), view.slack.subscriptions.len()),
            (1, 1),
            "the seeded subscriptions are what the read returns, so the rows above are not empty on both sides",
        );

        let other = ViewSurface::new(Arc::clone(&workspace)).connectors(Some("elsewhere"));
        assert!(
            other.gotify.subscriptions.is_empty() && other.slack.subscriptions.is_empty(),
            "another project's scope returns none of forge's rows",
        );
        let unscoped = ViewSurface::new(Arc::clone(&workspace)).connectors(None);
        assert!(
            unscoped.gotify.subscriptions.is_empty() && unscoped.slack.subscriptions.is_empty(),
            "an unstamped session is scoped to no project rather than every project",
        );
    }
}
