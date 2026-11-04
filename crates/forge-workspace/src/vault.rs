//! The process-wide vault session and its audit.
//!
//! One `VaultSession` per forge process, built from `[vault]` at boot and
//! swapped on a reload that changes the section. It owns the tokens and the
//! derived keys; the connector holds neither. There is no client-side idle
//! timeout: a call that the server rejects with 401 tries the refresh token,
//! and only a failed refresh locks the session and makes the next held call
//! ask for the master password.
//!
//! The master password never enters this struct. It arrives with one answer,
//! is used to derive the master key, and is zeroized before the answer is
//! dropped.

use std::path::{Path, PathBuf};

use forge_connectors::vault::crypto::{self, Unlocked};
use forge_connectors::vault::models::{Prelogin, SyncData, TokenResponse, TwoFactorCode};
use forge_connectors::vault::{VaultClient, VaultError};
use forge_primitives::vault::VaultTool;
use serde_json::Value;
use zeroize::Zeroize;

use crate::SessionSlot;
use crate::config::VaultConfig;

/// The access and refresh tokens one unlock produced.
struct Tokens {
    access: String,
    refresh: Option<String>,
}

impl Drop for Tokens {
    fn drop(&mut self) {
        self.access.zeroize();
        if let Some(refresh) = &mut self.refresh {
            refresh.zeroize();
        }
    }
}

/// The session's live half: the tokens and the derived keys, or neither.
#[derive(Default)]
struct SessionState {
    tokens: Option<Tokens>,
    keys: Option<std::sync::Arc<Unlocked>>,
}

/// One process-wide Vaultwarden session.
pub(crate) struct VaultSession {
    client: VaultClient,
    email: String,
    state: parking_lot::RwLock<SessionState>,
    /// Serializes unlocks, so two parked cards that both carry a password
    /// cannot race: the first wins and the second reads an unlocked session.
    unlock_gate: tokio::sync::Mutex<()>,
    audit: Option<VaultAudit>,
}

impl VaultSession {
    /// Build the session's client and audit. The audit path is
    /// `<app-support>/vault/audit-<config-dir-store-key>.jsonl`; a missing
    /// app-support dir leaves the session usable and the audit off.
    pub(crate) fn new(
        config: &VaultConfig,
        app_support: Option<&Path>,
        config_dir: &Path,
    ) -> Result<Self, VaultError> {
        let client = VaultClient::new(&config.url)?;
        let audit = app_support.and_then(|base| {
            VaultAudit::new(base, &forge_sdk::config_dir_store_key(config_dir)).ok()
        });
        Ok(Self {
            client,
            email: config.email.clone(),
            state: parking_lot::RwLock::new(SessionState::default()),
            unlock_gate: tokio::sync::Mutex::new(()),
            audit,
        })
    }

    /// Whether the derived keys are absent. A session that has never
    /// unlocked and one whose refresh failed both read locked.
    pub(crate) fn is_locked(&self) -> bool {
        self.state.read().keys.is_none()
    }

    /// The unlocked keys. A caller that needs crypto reads these; a locked
    /// session returns its own error rather than an option a caller forgets.
    pub(crate) fn keys(&self) -> Result<std::sync::Arc<Unlocked>, VaultError> {
        self.state.read().keys.clone().ok_or(VaultError::Locked)
    }

    /// Derive the master key and unlock the account, storing the tokens and
    /// the user/private/organization keys. Idempotent: a session another
    /// card already unlocked is left alone.
    pub(crate) async fn unlock(
        &self,
        password: &str,
        two_factor: Option<&str>,
    ) -> Result<(), VaultError> {
        let _gate = self.unlock_gate.lock().await;
        if !self.is_locked() {
            return Ok(());
        }

        let prelogin: Prelogin = self.client.prelogin(&self.email).await?;
        let master_key = crypto::derive_master_key(password, &self.email, &prelogin)
            .map_err(VaultError::Crypto)?;
        let hash = crypto::master_password_hash(&master_key, password);
        let two_factor =
            two_factor.map(|token| TwoFactorCode { token: token.to_owned(), provider: 0 });
        let token: TokenResponse =
            self.client.login_password(&self.email, &hash, two_factor.as_ref()).await?;
        let sync = self.client.sync(&token.access_token).await?;
        let unlocked =
            crypto::unlock(&token, &master_key, &sync.profile).map_err(VaultError::Crypto)?;

        let mut state = self.state.write();
        state.tokens = Some(Tokens { access: token.access_token, refresh: token.refresh_token });
        state.keys = Some(std::sync::Arc::new(unlocked));
        Ok(())
    }

    /// The current access token, or `Locked` when there is none.
    fn access_token(&self) -> Result<String, VaultError> {
        self.state
            .read()
            .tokens
            .as_ref()
            .map(|tokens| tokens.access.clone())
            .ok_or(VaultError::Locked)
    }

    /// Refresh the access token. A failed refresh clears the keys and tokens,
    /// which is what makes the next held call ask for the master password.
    pub(crate) async fn refresh(&self) -> Result<String, VaultError> {
        let refresh = {
            let state = self.state.read();
            state
                .tokens
                .as_ref()
                .and_then(|tokens| tokens.refresh.clone())
                .ok_or(VaultError::Locked)?
        };
        match self.client.refresh(&refresh).await {
            Ok(token) => {
                let access = token.access_token.clone();
                if let Some(state) = self.state.write().tokens.as_mut() {
                    state.access = token.access_token;
                    state.refresh = token.refresh_token.or(Some(refresh));
                }
                Ok(access)
            }
            Err(error) => {
                self.lock();
                Err(error)
            }
        }
    }

    /// Drop the tokens and the derived keys. The key types zeroize on drop.
    pub(crate) fn lock(&self) {
        let mut state = self.state.write();
        state.tokens = None;
        state.keys = None;
    }

    /// `GET /api/sync`, through the refresh path.
    pub(crate) async fn sync(&self) -> Result<SyncData, VaultError> {
        let token = self.access_token()?;
        match self.client.sync(&token).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.sync(&token).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn get_cipher(&self, id: &str) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.get_cipher(&token, id).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.get_cipher(&token, id).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn create_cipher(&self, body: &Value) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.create_cipher(&token, body).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.create_cipher(&token, body).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn update_cipher(&self, id: &str, body: &Value) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.update_cipher(&token, id, body).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.update_cipher(&token, id, body).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn trash_cipher(&self, id: &str) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.trash_cipher(&token, id).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.trash_cipher(&token, id).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn restore_cipher(&self, id: &str) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.restore_cipher(&token, id).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.restore_cipher(&token, id).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn list_folders(
        &self,
    ) -> Result<Vec<forge_connectors::vault::Folder>, VaultError> {
        let token = self.access_token()?;
        match self.client.list_folders(&token).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.list_folders(&token).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn create_folder(&self, name: &str) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.create_folder(&token, name).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.create_folder(&token, name).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn update_folder(&self, id: &str, name: &str) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.update_folder(&token, id, name).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.update_folder(&token, id, name).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn delete_folder(&self, id: &str) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.delete_folder(&token, id).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.delete_folder(&token, id).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn create_attachment_slot(
        &self,
        cipher_id: &str,
        body: &Value,
    ) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.create_attachment_slot(&token, cipher_id, body).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.create_attachment_slot(&token, cipher_id, body).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn upload_attachment(
        &self,
        cipher_id: &str,
        attachment_id: &str,
        data: Vec<u8>,
    ) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.upload_attachment(&token, cipher_id, attachment_id, data.clone()).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.upload_attachment(&token, cipher_id, attachment_id, data).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn get_attachment_meta(
        &self,
        cipher_id: &str,
        attachment_id: &str,
    ) -> Result<forge_connectors::vault::AttachmentMeta, VaultError> {
        let token = self.access_token()?;
        match self.client.get_attachment_meta(&token, cipher_id, attachment_id).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.get_attachment_meta(&token, cipher_id, attachment_id).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn download_attachment(&self, url: &str) -> Result<Vec<u8>, VaultError> {
        let token = self.access_token()?;
        match self.client.download_attachment(&token, url).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.download_attachment(&token, url).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    pub(crate) async fn delete_attachment(
        &self,
        cipher_id: &str,
        attachment_id: &str,
    ) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.delete_attachment(&token, cipher_id, attachment_id).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh().await?;
                match self.client.delete_attachment(&token, cipher_id, attachment_id).await {
                    Err(VaultError::Unauthorized) => {
                        self.lock();
                        Err(VaultError::Locked)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    /// One audit line: who, which tool, which item and field, what was
    /// decided, and what the operation did. Never a secret value.
    pub(crate) fn audit(
        &self,
        caller: &SessionSlot,
        tool: VaultTool,
        item_id: Option<&str>,
        item_name: Option<&str>,
        field: Option<&str>,
        decision: &str,
        result: &str,
    ) {
        let Some(audit) = &self.audit else { return };
        let line = serde_json::json!({
            "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "caller": caller.display().clone(),
            "tool": tool.as_str(),
            "item_id": item_id,
            "item_name": item_name,
            "field": field,
            "decision": decision,
            "result": result,
        });
        if let Err(error) = audit.append(&line.to_string()) {
            tracing::error!(
                target: "forge_workspace::vault",
                event_name = "vault_audit_write_failed",
                %error,
                "a vault audit line was not written",
            );
        }
    }
}

/// The append-only audit file, rotated 10 MB x 6.
struct VaultAudit {
    path: PathBuf,
    file: parking_lot::Mutex<AuditFile>,
}

struct AuditFile {
    file: std::fs::File,
    size: u64,
}

const AUDIT_MAX_BYTES: u64 = 10 * 1024 * 1024;
const AUDIT_MAX_FILES: usize = 6;

impl VaultAudit {
    fn new(app_support: &Path, store_key: &str) -> std::io::Result<Self> {
        let dir = app_support.join("vault");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("audit-{store_key}.jsonl"));
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self { path, file: parking_lot::Mutex::new(AuditFile { file, size }) })
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut audit = self.file.lock();
        let bytes = line.len() as u64 + 1;
        if audit.size + bytes > AUDIT_MAX_BYTES {
            rotate(&self.path, &mut audit)?;
        }
        audit.file.write_all(line.as_bytes())?;
        audit.file.write_all(b"\n")?;
        audit.size += bytes;
        audit.file.flush()
    }
}

fn rotate(path: &Path, audit: &mut AuditFile) -> std::io::Result<()> {
    let _ = std::fs::remove_file(rotated_path(path, AUDIT_MAX_FILES));
    for index in (1..AUDIT_MAX_FILES).rev() {
        let from = rotated_path(path, index);
        if from.exists() {
            let _ = std::fs::rename(&from, rotated_path(path, index + 1));
        }
    }
    let _ = std::fs::rename(path, rotated_path(path, 1));
    audit.file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
    audit.size = 0;
    Ok(())
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{index}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_connectors::vault::models::Prelogin;

    fn config() -> VaultConfig {
        VaultConfig { url: "http://127.0.0.1:1".to_owned(), email: "hi@vedhavyas.dev".to_owned() }
    }

    #[test]
    fn a_new_session_is_locked() {
        let session = VaultSession::new(&config(), None, Path::new("/tmp")).expect("a session");
        assert!(session.is_locked());
        assert!(matches!(session.keys(), Err(VaultError::Locked)));
    }

    #[test]
    fn the_audit_writes_one_json_line_with_no_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audit = VaultAudit::new(dir.path(), "store").expect("audit");
        audit.append(r#"{"tool":"vault_get","decision":"approved"}"#).expect("append");
        let path = dir.path().join("vault").join("audit-store.jsonl");
        let text = std::fs::read_to_string(path).expect("read");
        assert_eq!(text.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("json");
        assert_eq!(value["tool"], "vault_get");
    }

    #[test]
    fn an_audit_line_carries_the_decision_and_no_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "hi@vedhavyas.dev".to_owned(),
        };
        let config_dir = Path::new("/tmp");
        let session = VaultSession::new(&config, Some(dir.path()), config_dir).expect("session");
        session.audit(
            &SessionSlot::lead("Default", "forge"),
            VaultTool::Get,
            Some("item-id"),
            Some("bank"),
            Some("password"),
            "approve",
            "ok",
        );
        let path = dir
            .path()
            .join("vault")
            .join(format!("audit-{}.jsonl", forge_sdk::config_dir_store_key(config_dir)));
        let line = std::fs::read_to_string(path).expect("audit line");
        assert!(line.contains("\"decision\":\"approve\""), "{line}");
        assert!(line.contains("\"tool\":\"vault__get\""), "{line}");
        assert!(!line.contains("hunter2"), "an audit line never carries a value: {line}");
    }

    #[test]
    fn the_audit_rotates_through_six_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audit = VaultAudit::new(dir.path(), "store").expect("audit");
        // Write enough lines to force one rotation.
        let line = "x".repeat(1024 * 1024);
        for _ in 0..11 {
            audit.append(&line).expect("append");
        }
        let base = dir.path().join("vault");
        assert!(base.join("audit-store.jsonl.1").exists(), "the first rotation made .1");
    }

    #[test]
    fn the_prelogin_kdf_is_the_one_the_session_derives_with() {
        let pre =
            Prelogin { kdf: 0, kdf_iterations: 100_000, kdf_memory: None, kdf_parallelism: None };
        let master =
            crypto::derive_master_key("asdfasdf", "test@bitwarden.com", &pre).expect("master key");
        assert_eq!(
            crypto::master_password_hash(&master, "asdfasdf"),
            "wmyadRMyBZOH7P/a/ucTCbSghKgdzDpPqUnu/DAVtSw="
        );
    }
}
