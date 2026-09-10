//! The Slack seam on [`Workspace`]: one client per configured workspace,
//! the subscription records sessions create, and the boot verification
//! that proves each token.
//!
//! Everything here stays on `Workspace` as a second `impl` block, the
//! way [`crate::gotify`] does, so the boot path and the `mcp::slack`
//! facade keep their paths. The client itself lives in
//! `forge_connectors::slack`; this module holds the workspace state.

use std::collections::BTreeMap;
use std::sync::Arc;

use forge_connectors::slack::{AuthTest, SlackClient};
use forge_primitives::slack::SlackConfig;
use uuid::Uuid;

use crate::workspace::Workspace;

/// One client per configured workspace, keyed by its label. `Debug` is
/// safe to derive: [`SlackClient`]'s own `Debug` prints no token.
#[derive(Debug, Default)]
pub struct SlackWorkspaces {
    clients: BTreeMap<String, SlackClient>,
}

impl SlackWorkspaces {
    /// Labels address workspaces in the MCP tools and the Inspector, so
    /// they must be distinct and the token must be present.
    pub fn from_config(configs: &[SlackConfig], http: &reqwest::Client) -> Result<Self, String> {
        let mut clients = BTreeMap::new();
        for config in configs {
            let label = config.workspace.trim();
            if label.is_empty() {
                return Err("a [[slack]] entry has an empty workspace label".to_owned());
            }
            if config.token.trim().is_empty() {
                return Err(format!("slack workspace '{label}' has an empty token"));
            }
            if clients.contains_key(label) {
                return Err(format!("two [[slack]] entries share the label '{label}'"));
            }
            clients.insert(label.to_owned(), SlackClient::new(http.clone(), config.token.clone()));
        }
        Ok(Self { clients })
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Every configured label, sorted (the map is ordered).
    pub fn labels(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    pub fn client(&self, label: &str) -> Option<&SlackClient> {
        self.clients.get(label)
    }

    /// Prove every token at boot. A failure is reported, never fatal:
    /// one dead workspace must not stop forge from booting.
    pub async fn verify_all(&self) -> Vec<(String, Result<AuthTest, String>)> {
        let mut out = Vec::new();
        for (label, client) in &self.clients {
            let result = client.auth_test().await.map_err(|err| err.to_string());
            out.push((label.clone(), result));
        }
        out
    }
}

impl Workspace {
    /// Register a Slack subscription in the active set. Durable ones also
    /// persist to the redb store; ephemeral ad-hoc-worker ones stay in
    /// memory only and drop on restart.
    pub(crate) fn add_slack_subscription(
        &self,
        sub: forge_primitives::slack::SlackSubscription,
        durable: bool,
    ) {
        if durable
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::slack::insert(db, &sub)
        {
            tracing::warn!(
                target: "forge_workspace::slack",
                %error,
                "persisting a Slack subscription failed",
            );
        }
        self.slack_subs.lock().push(sub);
    }

    /// Every Slack subscription owned in `project`, whichever session
    /// owns it. Backs the phase 3 pump, which needs the whole set.
    pub fn slack_subscriptions_for_project(
        &self,
        project: &str,
    ) -> Vec<forge_primitives::slack::SlackSubscription> {
        self.slack_subs.lock().iter().filter(|s| s.project == project).cloned().collect()
    }

    /// Remove the subscription `id` in `project` only when its owner
    /// matches `owner` (`None` = a lead subscription, `Some(label)` =
    /// that worker's), from both the active set and the redb store.
    /// Returns whether an entry was removed. Backs the owner-scoped
    /// `slack__unsubscribe` so a caller removes only what it subscribed.
    pub(crate) fn remove_slack_subscription_owned_by(
        &self,
        project: &str,
        id: Uuid,
        owner: Option<&str>,
    ) -> bool {
        let removed = {
            let mut subs = self.slack_subs.lock();
            let before = subs.len();
            subs.retain(|s| {
                !(s.id == id && s.project == project && s.team_role.as_deref() == owner)
            });
            subs.len() != before
        };
        if removed
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::slack::remove(db, id)
        {
            tracing::warn!(
                target: "forge_workspace::slack",
                %error,
                "removing a persisted Slack subscription failed",
            );
        }
        removed
    }

    /// Prove each workspace's token once, logging the team it belongs to
    /// or the failure. Idempotent, and a no-op with no `[[slack]]` entry.
    pub fn start_slack_verification(self: &Arc<Self>) {
        if self.slack.is_empty() {
            return;
        }
        if self.slack_verification_started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let slack = Arc::clone(&self.slack);
        tokio::spawn(async move {
            for (label, result) in slack.verify_all().await {
                match result {
                    Ok(auth) => tracing::info!(
                        target: "forge_workspace::slack",
                        workspace = %label,
                        team = %auth.team,
                        user = %auth.user,
                        "slack workspace verified",
                    ),
                    Err(error) => tracing::warn!(
                        target: "forge_workspace::slack",
                        workspace = %label,
                        %error,
                        "slack auth.test failed; this workspace stays dormant",
                    ),
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::slack::{SlackSubscription, SlackSubscriptionTarget};
    use uuid::Uuid;

    fn cfg(workspace: &str, token: &str) -> SlackConfig {
        SlackConfig { workspace: workspace.to_owned(), token: token.to_owned(), poll_seconds: 30 }
    }

    fn sub_for(project: &str, team_role: Option<&str>) -> SlackSubscription {
        SlackSubscription {
            id: Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: project.to_owned(),
            team_role: team_role.map(str::to_owned),
            target: SlackSubscriptionTarget::DirectMessages,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_subscription_is_scoped_to_its_project_and_owner() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.add_slack_subscription(sub_for("forge", None), true);
        ws.add_slack_subscription(sub_for("forge", Some("tester")), true);
        ws.add_slack_subscription(sub_for("other", None), true);

        let visible = ws.slack_subscriptions_for_project("forge");
        assert_eq!(visible.len(), 2, "another project's subscription must not leak in");
        assert!(visible.iter().any(|s| s.team_role.is_none()));
        assert!(visible.iter().any(|s| s.team_role.as_deref() == Some("tester")));
    }

    #[test]
    fn a_worker_cannot_remove_another_owners_subscription() {
        let (ws, _rx) = Workspace::testing_stub();
        let lead = sub_for("forge", None);
        ws.add_slack_subscription(lead.clone(), true);

        assert!(
            !ws.remove_slack_subscription_owned_by("forge", lead.id, Some("tester")),
            "a worker must not remove the lead's subscription",
        );
        assert_eq!(
            ws.slack_subscriptions_for_project("forge").len(),
            1,
            "a refused removal removes nothing",
        );
        assert!(ws.remove_slack_subscription_owned_by("forge", lead.id, None));
        assert!(ws.slack_subscriptions_for_project("forge").is_empty());
    }

    #[test]
    fn duplicate_workspace_labels_are_refused() {
        let configs = vec![cfg("acme", "xoxp-test"), cfg("acme", "xoxp-other")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("two workspaces sharing a label cannot be addressed apart");
        assert!(err.contains("acme"), "the error has to name the shared label, got: {err}");
    }

    #[test]
    fn an_empty_token_is_refused() {
        let configs = vec![cfg("acme", "")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("a blank token would fail on every call");
        assert!(
            err.contains("acme") && err.contains("empty token"),
            "the error has to name the workspace and the missing credential, got: {err}",
        );
    }

    #[test]
    fn an_empty_workspace_label_is_refused() {
        let configs = vec![cfg("", "xoxp-test")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("a workspace with no label cannot be addressed at all");
        assert!(
            err.contains("empty workspace label"),
            "the error has to name the label, got: {err}",
        );
    }
}
