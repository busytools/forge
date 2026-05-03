//! Recording [`AgentBridge`] for tests.
//!
//! Production code talks to `forge_sdk::Client` through
//! [`super::forge_sdk_bridge::ForgeSdkBridge`]. Tests don't want to
//! spawn a CLI subprocess, so they swap in [`RecordingBridge`]: every
//! trait method pushes a [`ForgeSdkCommand`] record onto the channel
//! the test owns, and the test asserts on it directly via
//! `rx.try_recv()`.
//!
//! This module is `pub` so integration tests under
//! `crates/forge-tui/tests/` can pull the type in without a
//! `#[cfg(test)]` gate; everything inside is `#[allow(dead_code)]`
//! because production code never reads the recorded variants.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::client::{AgentBridge, AgentEvent, PromptResponse, SessionLaunchSettings};
use crate::agent::types::{
    ElicitationAction, McpServerConfig, PermissionOutcome, PromptChunk, QuestionOutcome,
};

/// Record of a method call on [`RecordingBridge`]. Mirrors the trait
/// surface — tests match on a variant to assert the App invoked the
/// right call.
#[derive(Debug, Clone, PartialEq)]
pub enum ForgeSdkCommand {
    Prompt {
        session_id: String,
        chunks: Vec<PromptChunk>,
    },
    Cancel {
        session_id: String,
    },
    SetMode {
        session_id: String,
        mode: String,
    },
    SetModel {
        session_id: String,
        model: String,
    },
    GenerateSessionTitle {
        session_id: String,
        description: String,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    GetStatusSnapshot {
        session_id: String,
    },
    GetOauthCredentialsSnapshot {
        session_id: String,
    },
    StartGitContextWatch {
        session_id: String,
        cwd: PathBuf,
    },
    StopGitContextWatch {
        session_id: String,
    },
    GetContextUsage {
        session_id: String,
    },
    ReloadPlugins {
        session_id: String,
    },
    GetMcpSnapshot {
        session_id: String,
    },
    RespondToElicitation {
        session_id: String,
        elicitation_request_id: String,
        action: ElicitationAction,
        content: Option<Value>,
    },
    ReconnectMcpServer {
        session_id: String,
        server_name: String,
    },
    ToggleMcpServer {
        session_id: String,
        server_name: String,
        enabled: bool,
    },
    SetMcpServers {
        session_id: String,
        servers: BTreeMap<String, McpServerConfig>,
    },
    AuthenticateMcpServer {
        session_id: String,
        server_name: String,
    },
    ClearMcpAuth {
        session_id: String,
        server_name: String,
    },
    SubmitMcpOauthCallbackUrl {
        session_id: String,
        server_name: String,
        callback_url: String,
    },
    NewSession {
        cwd: String,
        launch_settings: SessionLaunchSettings,
    },
    ResumeSession {
        session_id: String,
        launch_settings: SessionLaunchSettings,
    },
    PermissionResponse {
        session_id: String,
        tool_call_id: String,
        outcome: PermissionOutcome,
    },
    QuestionResponse {
        session_id: String,
        tool_call_id: String,
        outcome: QuestionOutcome,
    },
}

/// Test fixture: implements [`AgentBridge`] by pushing a
/// [`ForgeSdkCommand`] record per call onto an mpsc channel.
#[derive(Clone)]
pub struct RecordingBridge {
    tx: mpsc::UnboundedSender<ForgeSdkCommand>,
}

impl RecordingBridge {
    #[must_use]
    pub fn new(tx: mpsc::UnboundedSender<ForgeSdkCommand>) -> Self {
        Self { tx }
    }

    fn record(&self, cmd: ForgeSdkCommand) -> anyhow::Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("recording bridge: receiver dropped"))
    }
}

#[async_trait(?Send)]
impl AgentBridge for RecordingBridge {
    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {
        // Tests don't drive the App's event loop through this bridge;
        // they construct it solely to capture outbound commands.
        None
    }

    fn prompt_text(&self, session_id: String, text: String) -> anyhow::Result<PromptResponse> {
        self.prompt_with_images(session_id, text, Vec::new())
    }

    fn prompt_with_images(
        &self,
        session_id: String,
        text: String,
        images: Vec<crate::app::clipboard_image::ImageAttachment>,
    ) -> anyhow::Result<PromptResponse> {
        let mut chunks: Vec<PromptChunk> = Vec::with_capacity(1 + images.len());
        for img in images {
            if crate::app::clipboard_image::validate_image(&img.data, &img.mime_type).is_err() {
                continue;
            }
            chunks.push(PromptChunk {
                kind: "image".to_owned(),
                value: serde_json::json!({
                    "data": img.data,
                    "mime_type": img.mime_type,
                }),
            });
        }
        chunks.push(PromptChunk {
            kind: "text".to_owned(),
            value: Value::String(text),
        });
        self.record(ForgeSdkCommand::Prompt { session_id, chunks })?;
        Ok(PromptResponse { stop_reason: "end_turn".to_owned() })
    }

    fn cancel(&self, session_id: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::Cancel { session_id })
    }

    fn set_mode(&self, session_id: String, mode: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::SetMode { session_id, mode })
    }

    fn set_model(&self, session_id: String, model: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::SetModel { session_id, model })
    }

    fn generate_session_title(
        &self,
        session_id: String,
        description: String,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::GenerateSessionTitle { session_id, description })
    }

    fn rename_session(&self, session_id: String, title: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::RenameSession { session_id, title })
    }

    fn get_status_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::GetStatusSnapshot { session_id })
    }

    fn get_oauth_credentials_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::GetOauthCredentialsSnapshot { session_id })
    }

    fn start_git_context_watch(&self, session_id: String, cwd: PathBuf) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::StartGitContextWatch { session_id, cwd })
    }

    fn stop_git_context_watch(&self, session_id: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::StopGitContextWatch { session_id })
    }

    fn get_context_usage(&self, session_id: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::GetContextUsage { session_id })
    }

    fn reload_plugins(&self, session_id: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::ReloadPlugins { session_id })
    }

    fn get_mcp_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::GetMcpSnapshot { session_id })
    }

    fn respond_to_elicitation(
        &self,
        session_id: String,
        elicitation_request_id: String,
        action: ElicitationAction,
        content: Option<Value>,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::RespondToElicitation {
            session_id,
            elicitation_request_id,
            action,
            content,
        })
    }

    fn reconnect_mcp_server(&self, session_id: String, server_name: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::ReconnectMcpServer { session_id, server_name })
    }

    fn toggle_mcp_server(
        &self,
        session_id: String,
        server_name: String,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::ToggleMcpServer { session_id, server_name, enabled })
    }

    fn set_mcp_servers(
        &self,
        session_id: String,
        servers: BTreeMap<String, McpServerConfig>,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::SetMcpServers { session_id, servers })
    }

    fn authenticate_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::AuthenticateMcpServer { session_id, server_name })
    }

    fn clear_mcp_auth(&self, session_id: String, server_name: String) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::ClearMcpAuth { session_id, server_name })
    }

    fn submit_mcp_oauth_callback_url(
        &self,
        session_id: String,
        server_name: String,
        callback_url: String,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::SubmitMcpOauthCallbackUrl {
            session_id,
            server_name,
            callback_url,
        })
    }

    fn new_session(
        &self,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::NewSession { cwd, launch_settings })
    }

    fn resume_session(
        &self,
        session_id: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::ResumeSession { session_id, launch_settings })
    }

    fn permission_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: PermissionOutcome,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::PermissionResponse { session_id, tool_call_id, outcome })
    }

    fn question_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: QuestionOutcome,
    ) -> anyhow::Result<()> {
        self.record(ForgeSdkCommand::QuestionResponse { session_id, tool_call_id, outcome })
    }

    fn config_dir(&self) -> PathBuf {
        forge_sdk::claude_config_dir()
    }

    fn project_memory_path(&self, cwd: &Path) -> PathBuf {
        forge_sdk::project_memory_path(cwd)
    }

    fn oauth_credentials(&self) -> Option<forge_sdk::OauthCredentials> {
        forge_sdk::oauth_credentials()
    }

    fn settings_documents(&self, cwd: &Path) -> forge_sdk::SettingsDocuments {
        forge_sdk::settings_documents(cwd)
    }

    fn write_settings_document(
        &self,
        target: &forge_sdk::SettingsTarget,
        document: &Value,
    ) -> Result<(), forge_sdk::Error> {
        forge_sdk::write_settings_document(target, document)
    }

    async fn oauth_usage(&self) -> Result<forge_sdk::OauthUsage, forge_sdk::OauthUsageError> {
        forge_sdk::oauth_usage().await
    }
}
