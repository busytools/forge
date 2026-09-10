//! `SlackFacade` - the seam between the Slack MCP tools and workspace
//! state. The production impl ([`ProdSlackFacade`]) resolves the
//! requested workspace label and drives that workspace's client; the
//! mock returns preloaded results so the tool tests can assert argument
//! handling and error surfacing without a real workspace.

use std::sync::{Arc, Weak};

use forge_primitives::slack::SlackConversation;

use crate::workspace::Workspace;

/// Why `slack__list` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackListError {
    /// No `[[slack]]` entry in forge.toml.
    NotConfigured,
    /// The caller named a workspace that is not configured. Carries the
    /// labels that are, so the error can list them.
    UnknownWorkspace { requested: String, known: Vec<String> },
    /// Several workspaces are configured and the caller named none.
    WorkspaceRequired { known: Vec<String> },
    /// The Web API call failed. Carries the formatted error for the LLM.
    Fetch(String),
}

/// The Slack tools' view of the workspace.
#[async_trait::async_trait]
pub(crate) trait SlackFacade: Send + Sync {
    /// Every conversation the named workspace's token is in. `None` picks
    /// the only configured workspace and errors when there is more than one.
    async fn conversations(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<SlackConversation>, SlackListError>;
}

/// Production facade over `Weak<Workspace>` (weak to avoid a cycle with
/// the MCP server the workspace owns).
pub(crate) struct ProdSlackFacade {
    workspace: Weak<Workspace>,
}

impl ProdSlackFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn SlackFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

#[async_trait::async_trait]
impl SlackFacade for ProdSlackFacade {
    async fn conversations(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<SlackConversation>, SlackListError> {
        let ws = self.workspace.upgrade().ok_or(SlackListError::NotConfigured)?;
        if ws.slack.is_empty() {
            return Err(SlackListError::NotConfigured);
        }
        let label = if let Some(label) = workspace {
            label.to_owned()
        } else {
            let labels = ws.slack.labels();
            if labels.len() != 1 {
                return Err(SlackListError::WorkspaceRequired { known: labels });
            }
            labels.into_iter().next().unwrap_or_default()
        };
        let client = ws.slack.client(&label).ok_or_else(|| SlackListError::UnknownWorkspace {
            requested: label.clone(),
            known: ws.slack.labels(),
        })?;
        client.list_conversations().await.map_err(|err| SlackListError::Fetch(err.to_string()))
    }
}

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool parses args and surfaces facade results/errors.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockSlackFacade {
    pub conversations_calls: parking_lot::Mutex<Vec<Option<String>>>,
    pub conversations_result:
        parking_lot::Mutex<Option<Result<Vec<SlackConversation>, SlackListError>>>,
}

#[cfg(test)]
impl MockSlackFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn into_arc(self) -> Arc<dyn SlackFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl SlackFacade for MockSlackFacade {
    async fn conversations(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<SlackConversation>, SlackListError> {
        self.conversations_calls.lock().push(workspace.map(str::to_owned));
        self.conversations_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }
}
