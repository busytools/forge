//! The Slack seam on [`Workspace`]: one client per configured workspace
//! and the boot verification that proves each token.
//!
//! Everything here stays on `Workspace` as a second `impl` block, the
//! way [`crate::gotify`] does, so the boot path and the `mcp::slack`
//! facade keep their paths. The client itself lives in
//! `forge_connectors::slack`; this module holds the workspace state.

use std::collections::BTreeMap;
use std::sync::Arc;

use forge_connectors::slack::{AuthTest, SlackClient};
use forge_primitives::slack::SlackConfig;

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

    fn cfg(workspace: &str, token: &str) -> SlackConfig {
        SlackConfig { workspace: workspace.to_owned(), token: token.to_owned(), poll_seconds: 30 }
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
