//! `AgentBridge` impl backed by `forge-sdk` running in-process.
//!
//! Drives a [`forge_sdk::Client`] directly — no Node.js subprocess,
//! no NDJSON, no command queue. The bridge holds the spawned
//! `Arc<Client>` and dispatches each trait method as a direct call
//! (or a `tokio::spawn`'d async task when the trait method is
//! fire-and-forget). Synthesized events (Connected, `PermissionRequest`,
//! `McpSnapshot`, …) flow back through an `mpsc::UnboundedSender<AgentEvent>`
//! the bridge owns; consumers grab the matching receiver once via
//! [`AgentBridge::take_events`].
//!
//! ```text
//!     TUI                     ForgeSdkBridge                  forge_sdk::Client
//!      | trait method            |                                    |
//!      |------------------------>|  client.method().await             |
//!      |                         |----------------------------------->|
//!      |                         |                                    |
//!      |       AgentEvent        |  reader_loop / callbacks           |
//!      |<------ event_tx --------+<-----------------------------------|
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge_sdk::Client;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::client::{AgentBridge, AgentEvent, PromptResponse, SessionLaunchSettings};
use crate::forge_sdk_worker;
use forge_primitives::{ElicitationAction, McpServerConfig, PermissionOutcome, QuestionOutcome};

/// Pending permission responses keyed by `tool_use_id`. The
/// `can_use_tool` callback parks a oneshot here when the CLI asks;
/// dispatch drains it when the matching `permission_response` arrives
/// from the App.
pub(crate) type PendingResponses =
    Arc<Mutex<HashMap<String, oneshot::Sender<forge_sdk::PermissionDecision>>>>;

/// Pending question outcomes keyed by `tool_use_id`. The
/// `AskUserQuestion` driver in the `can_use_tool` callback parks a
/// fresh oneshot per question, emits a `QuestionRequest`, and awaits
/// the matching `question_response`.
pub(crate) type PendingQuestions = Arc<Mutex<HashMap<String, oneshot::Sender<QuestionOutcome>>>>;

/// Forge-SDK-backed implementation of [`AgentBridge`].
///
/// Single instance per connection. The bridge owns the spawned
/// `forge_sdk::Client`, the `can_use_tool` parking lots, the per-cwd
/// git-context watchers, and the outbound `AgentEvent` channel.
#[derive(Clone)]
pub struct ForgeSdkBridge {
    inner: Arc<BridgeInner>,
}

pub(crate) struct BridgeInner {
    /// Set after first `new_session` / `resume_session`; cleared on
    /// session replace or shutdown.
    client: Mutex<Option<Client>>,
    /// Bridge → App event emission channel. Cloned freely into the
    /// reader subtask + `can_use_tool` callback closures.
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    /// Single-take receiver handed out via [`AgentBridge::take_events`].
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
    /// Permission round-trip parking lot.
    pub(crate) pending: PendingResponses,
    /// Question round-trip parking lot.
    pub(crate) pending_questions: PendingQuestions,
    /// Active git-context watcher tasks, keyed by `session_id`. Aborted
    /// on bridge drop or session replace.
    git_watchers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Current session id, shared with the `can_use_tool` callback so
    /// permission/question events carry the right `session_id`.
    pub(crate) session_id_slot: Arc<Mutex<String>>,
}

impl ForgeSdkBridge {
    /// Construct a fresh bridge. The internal event channel is created
    /// here; consumers grab the receiver once via
    /// [`AgentBridge::take_events`].
    #[must_use]
    pub fn new() -> Self {
        let (event_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(BridgeInner {
                client: Mutex::new(None),
                event_tx,
                events_rx: Mutex::new(Some(events_rx)),
                pending: Arc::new(Mutex::new(HashMap::new())),
                pending_questions: Arc::new(Mutex::new(HashMap::new())),
                git_watchers: Mutex::new(HashMap::new()),
                session_id_slot: Arc::new(Mutex::new(String::new())),
            }),
        }
    }

    pub(crate) fn event_tx(&self) -> &mpsc::UnboundedSender<AgentEvent> {
        &self.inner.event_tx
    }

    pub(crate) fn inner_pending(&self) -> &PendingResponses {
        &self.inner.pending
    }

    pub(crate) fn inner_pending_questions(&self) -> &PendingQuestions {
        &self.inner.pending_questions
    }

    pub(crate) fn session_id_slot_arc(&self) -> &Arc<Mutex<String>> {
        &self.inner.session_id_slot
    }

    fn client(&self) -> Option<Client> {
        self.inner.client.lock().ok().and_then(|c| c.clone())
    }

    pub(crate) fn set_client(&self, client: Client) {
        if let Ok(mut slot) = self.inner.client.lock() {
            *slot = Some(client);
        }
    }

    pub(crate) fn clear_client(&self) -> Option<Client> {
        self.inner.client.lock().ok().and_then(|mut c| c.take())
    }

    /// Spawn a fire-and-forget client call. Logs and drops on failure.
    fn dispatch<F, Fut>(&self, label: &'static str, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let Some(client) = self.client() else {
            return Err(anyhow::anyhow!(
                "forge-sdk bridge: {label} called before active session"
            ));
        };
        tokio::spawn(async move {
            if let Err(err) = f(client).await {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    label,
                    error = %err,
                    "forge-sdk bridge: dispatch failed",
                );
            }
        });
        Ok(())
    }

    /// Replace any existing git watcher for `session_id` with a new
    /// task that pumps `GitContextWatcher` snapshots into the event
    /// channel.
    fn install_git_watcher(&self, session_id: String, cwd: &Path) {
        // Abort any prior watcher for this session so notify cleans up
        // its OS-level subscriptions before we replace it.
        if let Ok(mut watchers) = self.inner.git_watchers.lock()
            && let Some(prev) = watchers.remove(&session_id)
        {
            prev.abort();
        }

        let mut watcher = match forge_sdk::GitContextWatcher::new(cwd) {
            Ok(watcher) => watcher,
            Err(err) => {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    session_id = %session_id,
                    cwd = %cwd.display(),
                    error = %err,
                    "failed to start git context watcher",
                );
                return;
            }
        };
        let event_tx = self.inner.event_tx.clone();
        let task_session_id = session_id.clone();
        let handle = tokio::spawn(async move {
            while let Some(context) = watcher.next_snapshot().await {
                if event_tx
                    .send(AgentEvent::GitContextSnapshot {
                        session_id: task_session_id.clone(),
                        context,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        if let Ok(mut watchers) = self.inner.git_watchers.lock() {
            watchers.insert(session_id, handle);
        }
    }

    fn stop_git_watcher(&self, session_id: &str) {
        if let Ok(mut watchers) = self.inner.git_watchers.lock()
            && let Some(handle) = watchers.remove(session_id)
        {
            handle.abort();
        }
    }
}

impl Default for ForgeSdkBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BridgeInner {
    fn drop(&mut self) {
        if let Ok(mut watchers) = self.git_watchers.lock() {
            for (_, handle) in watchers.drain() {
                handle.abort();
            }
        }
    }
}

#[async_trait(?Send)]
impl AgentBridge for ForgeSdkBridge {
    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {
        self.inner
            .events_rx
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    fn prompt_text(&self, session_id: String, text: String) -> anyhow::Result<PromptResponse> {
        self.prompt_with_images(session_id, text, Vec::new())
    }

    fn prompt_with_images(
        &self,
        _session_id: String,
        text: String,
        images: Vec<forge_primitives::ImageAttachment>,
    ) -> anyhow::Result<PromptResponse> {
        let mut chunks: Vec<forge_primitives::PromptChunk> = Vec::with_capacity(1 + images.len());
        for img in images {
            if let Err(reason) = forge_primitives::validate_image(&img.data, &img.mime_type) {
                tracing::warn!(
                    target: crate::logging::targets::APP_INPUT,
                    "forge-sdk bridge: skipping invalid image: {reason}"
                );
                continue;
            }
            chunks.push(forge_primitives::PromptChunk {
                kind: "image".to_owned(),
                value: serde_json::json!({
                    "data": img.data,
                    "mime_type": img.mime_type,
                }),
            });
        }
        chunks.push(forge_primitives::PromptChunk {
            kind: "text".to_owned(),
            value: Value::String(text),
        });
        self.dispatch("prompt", move |client| async move {
            forge_sdk_worker::send_prompt(&client, chunks).await
        })?;
        Ok(PromptResponse {
            stop_reason: "end_turn".to_owned(),
        })
    }

    fn cancel(&self, _session_id: String) -> anyhow::Result<()> {
        self.dispatch("cancel", |client| async move {
            client.interrupt().await?;
            Ok(())
        })
    }

    fn set_mode(&self, _session_id: String, mode: String) -> anyhow::Result<()> {
        let parsed = forge_sdk_worker::parse_permission_mode(&mode)?;
        self.dispatch("set_mode", move |client| async move {
            client.set_permission_mode(parsed).await?;
            Ok(())
        })
    }

    fn set_model(&self, _session_id: String, model: String) -> anyhow::Result<()> {
        self.dispatch("set_model", move |client| async move {
            client.set_model(Some(model.as_str())).await?;
            Ok(())
        })
    }

    fn generate_session_title(
        &self,
        _session_id: String,
        description: String,
    ) -> anyhow::Result<()> {
        self.dispatch("generate_session_title", move |client| async move {
            let _ = client.generate_session_title(&description).await?;
            Ok(())
        })
    }

    fn rename_session(&self, session_id: String, title: String) -> anyhow::Result<()> {
        // Offline disk mutation — no Client required.
        crate::userdata::catalog::mutations::rename_session(&session_id, &title, None)?;
        Ok(())
    }

    fn get_status_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("get_status_snapshot", move |client| async move {
            let account = client
                .account_info_from_init()
                .or_else(crate::cloud::auth_status::account_info_from_shell)
                .unwrap_or_default();
            let _ = event_tx.send(AgentEvent::StatusSnapshot {
                session_id,
                account,
            });
            Ok(())
        })
    }

    fn get_oauth_credentials_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("get_oauth_credentials_snapshot", move |client| async move {
            let credentials = client.oauth_credentials();
            let _ = event_tx.send(AgentEvent::OauthCredentialsSnapshot {
                session_id,
                credentials,
            });
            Ok(())
        })
    }

    fn get_context_usage(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("get_context_usage", move |client| async move {
            let usage = client.get_context_usage().await?;
            let percentage = forge_sdk_worker::clamp_percentage_to_u8(usage.percentage);
            let _ = event_tx.send(AgentEvent::ContextUsage {
                session_id,
                percentage: Some(percentage),
            });
            Ok(())
        })
    }

    fn reload_plugins(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("reload_plugins", move |client| async move {
            match client.reload_plugins().await {
                Ok(_) => {
                    let _ = event_tx.send(AgentEvent::RuntimeReloadCompleted { session_id });
                }
                Err(e) => {
                    let _ = event_tx.send(AgentEvent::RuntimeReloadFailed {
                        session_id,
                        message: format!("reload_plugins failed: {e}"),
                    });
                }
            }
            Ok(())
        })
    }

    fn get_mcp_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("get_mcp_snapshot", move |client| async move {
            let response = client.mcp_status().await?;
            let _ = event_tx.send(AgentEvent::McpSnapshot {
                session_id,
                servers: response.mcp_servers,
                error: None,
            });
            Ok(())
        })
    }

    fn respond_to_elicitation(
        &self,
        _session_id: String,
        elicitation_request_id: String,
        action: ElicitationAction,
        content: Option<Value>,
    ) -> anyhow::Result<()> {
        let action_str = match action {
            ElicitationAction::Accept => "accept",
            ElicitationAction::Decline => "decline",
            ElicitationAction::Cancel => "cancel",
        };
        self.dispatch("respond_to_elicitation", move |client| async move {
            client
                .respond_to_elicitation(&elicitation_request_id, action_str, content)
                .await?;
            Ok(())
        })
    }

    fn reconnect_mcp_server(&self, session_id: String, server_name: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("reconnect_mcp_server", move |client| async move {
            if let Err(e) = client.mcp_reconnect(&server_name).await {
                let _ = event_tx.send(AgentEvent::McpOperationError {
                    session_id,
                    error: forge_primitives::McpOperationError {
                        operation: "reconnect".to_owned(),
                        server_name: Some(server_name),
                        message: format!("{e}"),
                    },
                });
            }
            Ok(())
        })
    }

    fn toggle_mcp_server(
        &self,
        session_id: String,
        server_name: String,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("toggle_mcp_server", move |client| async move {
            if let Err(e) = client.mcp_toggle(&server_name, enabled).await {
                let _ = event_tx.send(AgentEvent::McpOperationError {
                    session_id,
                    error: forge_primitives::McpOperationError {
                        operation: "toggle".to_owned(),
                        server_name: Some(server_name),
                        message: format!("{e}"),
                    },
                });
            }
            Ok(())
        })
    }

    fn set_mcp_servers(
        &self,
        session_id: String,
        servers: std::collections::BTreeMap<String, McpServerConfig>,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let payload = serde_json::to_value(servers)?;
        self.dispatch("set_mcp_servers", move |client| async move {
            if let Err(e) = client.mcp_set_servers(payload).await {
                let _ = event_tx.send(AgentEvent::McpOperationError {
                    session_id,
                    error: forge_primitives::McpOperationError {
                        operation: "set_servers".to_owned(),
                        server_name: None,
                        message: format!("{e}"),
                    },
                });
            }
            Ok(())
        })
    }

    fn authenticate_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("authenticate_mcp_server", move |client| async move {
            match client.mcp_authenticate(&server_name).await {
                Ok(response) => {
                    let url = response
                        .get("redirect_url")
                        .or_else(|| response.get("authUrl"))
                        .or_else(|| response.get("auth_url"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    if let Some(auth_url) = url {
                        let _ = event_tx.send(AgentEvent::McpAuthRedirect {
                            session_id,
                            redirect: forge_primitives::McpAuthRedirect {
                                server_name,
                                auth_url,
                                requires_user_action: true,
                            },
                        });
                    }
                }
                Err(e) => {
                    let _ = event_tx.send(AgentEvent::McpOperationError {
                        session_id,
                        error: forge_primitives::McpOperationError {
                            operation: "authenticate".to_owned(),
                            server_name: Some(server_name),
                            message: format!("{e}"),
                        },
                    });
                }
            }
            Ok(())
        })
    }

    fn clear_mcp_auth(&self, session_id: String, server_name: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("clear_mcp_auth", move |client| async move {
            if let Err(e) = client.mcp_clear_auth(&server_name).await {
                let _ = event_tx.send(AgentEvent::McpOperationError {
                    session_id,
                    error: forge_primitives::McpOperationError {
                        operation: "clear_auth".to_owned(),
                        server_name: Some(server_name),
                        message: format!("{e}"),
                    },
                });
            }
            Ok(())
        })
    }

    fn submit_mcp_oauth_callback_url(
        &self,
        session_id: String,
        server_name: String,
        callback_url: String,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("submit_mcp_oauth_callback_url", move |client| async move {
            if let Err(e) = client
                .mcp_oauth_callback_url(&server_name, &callback_url)
                .await
            {
                let _ = event_tx.send(AgentEvent::McpOperationError {
                    session_id,
                    error: forge_primitives::McpOperationError {
                        operation: "oauth_callback".to_owned(),
                        server_name: Some(server_name),
                        message: format!("{e}"),
                    },
                });
            }
            Ok(())
        })
    }

    fn new_session(
        &self,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let bridge = self.clone();
        tokio::spawn(async move {
            if let Err(err) =
                forge_sdk_worker::spawn_session(&bridge, &cwd, None, &launch_settings).await
            {
                let _ = bridge.event_tx().send(AgentEvent::ConnectionFailed {
                    message: format!("forge-sdk session spawn failed: {err}"),
                });
            }
        });
        Ok(())
    }

    fn resume_session(
        &self,
        session_id: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let bridge = self.clone();
        tokio::spawn(async move {
            if let Err(err) =
                forge_sdk_worker::spawn_session(&bridge, "", Some(&session_id), &launch_settings)
                    .await
            {
                let _ = bridge.event_tx().send(AgentEvent::ConnectionFailed {
                    message: format!("forge-sdk session resume failed: {err}"),
                });
            }
        });
        Ok(())
    }

    fn permission_response(
        &self,
        _session_id: String,
        tool_call_id: String,
        outcome: PermissionOutcome,
    ) -> anyhow::Result<()> {
        forge_sdk_worker::deliver_permission_response(&self.inner.pending, &tool_call_id, outcome);
        Ok(())
    }

    fn question_response(
        &self,
        _session_id: String,
        tool_call_id: String,
        outcome: QuestionOutcome,
    ) -> anyhow::Result<()> {
        forge_sdk_worker::deliver_question_response(
            &self.inner.pending_questions,
            &tool_call_id,
            outcome,
        );
        Ok(())
    }

    fn start_git_context_watch(&self, session_id: String, cwd: PathBuf) -> anyhow::Result<()> {
        self.install_git_watcher(session_id, &cwd);
        Ok(())
    }

    fn stop_git_context_watch(&self, session_id: String) -> anyhow::Result<()> {
        self.stop_git_watcher(&session_id);
        Ok(())
    }

    // ---- Direct-return accessors (delegate to forge_sdk::*) ----

    fn config_dir(&self) -> PathBuf {
        forge_sdk::claude_config_dir()
    }

    fn project_memory_path(&self, cwd: &Path) -> PathBuf {
        crate::userdata::memory::project_memory_path(cwd)
    }

    fn oauth_credentials(&self) -> Option<forge_sdk::OauthCredentials> {
        forge_sdk::oauth_credentials()
    }

    fn settings_documents(&self, cwd: &Path) -> crate::userdata::settings::SettingsDocuments {
        crate::userdata::settings::settings_documents(cwd)
    }

    fn write_settings_document(
        &self,
        target: &crate::userdata::settings::SettingsTarget,
        document: &Value,
    ) -> Result<(), forge_sdk::Error> {
        crate::userdata::settings::write_settings_document(target, document)
    }

    async fn oauth_usage(&self) -> Result<forge_sdk::OauthUsage, forge_sdk::OauthUsageError> {
        forge_sdk::oauth_usage().await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn take_events_returns_some_once_then_none() {
        let bridge = ForgeSdkBridge::new();
        assert!(bridge.take_events().is_some());
        assert!(bridge.take_events().is_none());
    }

    #[test]
    fn dispatch_without_client_returns_error() {
        let bridge = ForgeSdkBridge::new();
        let err = bridge.cancel("session-1".to_owned()).unwrap_err();
        assert!(err.to_string().contains("before active session"));
    }

    #[test]
    fn rename_session_runs_offline_without_client() {
        let bridge = ForgeSdkBridge::new();
        // Bogus session id — `rename_session` propagates the disk
        // error rather than the "no active session" guard. The point
        // of this test is to confirm we do NOT take the dispatch path.
        let err = bridge
            .rename_session("does-not-exist-session-id".to_owned(), "title".to_owned())
            .unwrap_err();
        // Whatever forge_sdk surfaces — just ensure it isn't the
        // bridge's own "no active session" message.
        assert!(!err.to_string().contains("before active session"));
    }
}
