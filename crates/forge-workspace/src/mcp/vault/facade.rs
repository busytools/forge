//! The vault family's facade: the thirteen operations the tools call, every
//! one held for the reader's approval.
//!
//! The facade owns no session state: the process-wide `VaultSession` does.
//! It parks a `VaultDraft` on the workspace's registry, awaits one answer,
//! and only then unlocks (when the card asked for the master password) and
//! runs the operation. The card draws only what the draft carries; no secret
//! value is put on it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use forge_connectors::vault::VaultError;
use forge_connectors::vault::crypto::Unlocked;
use forge_connectors::vault::models::SyncData;
use forge_primitives::vault::{
    VaultAskDecision, VaultAskEnding, VaultAttachmentDraft, VaultDraft, VaultTool,
};
use serde::Deserialize;
use serde_json::{Value, json};
use zeroize::Zeroize;

use crate::SessionSlot;
use crate::vault::VaultSession;
use crate::workspace::Workspace;

/// Why a vault tool call did not return a result.
#[derive(Debug, thiserror::Error)]
pub enum VaultToolError {
    #[error("{0}")]
    Message(String),
    #[error("the vault call was declined, so nothing ran")]
    Rejected,
    #[error("the vault call was sent back: {note}")]
    SentBack { note: String },
    #[error("the vault session is locked; the next call asks for the master password")]
    Locked,
    #[error("the vault is not configured: add a [vault] section to forge.toml")]
    NotConfigured,
    #[error("no client is attached to answer the vault call")]
    NoClient,
    #[error("the answerer went away before the vault call was settled")]
    Abandoned,
    #[error("vault request failed: {0}")]
    Vault(#[from] VaultError),
    #[error("vault crypto failed: {0}")]
    Crypto(#[from] anyhow::Error),
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for VaultToolError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// A custom field as `vault_create` / `vault_update` take it.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomFieldInput {
    pub name: String,
    pub value: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// The arguments `vault_create` accepts.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateInput {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    pub name: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub cardholder_name: Option<String>,
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub exp_month: Option<String>,
    #[serde(default)]
    pub exp_year: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub brand: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub custom_fields: Option<Vec<CustomFieldInput>>,
}

/// The fields `vault_update` may change.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PatchInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub cardholder_name: Option<String>,
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub exp_month: Option<String>,
    #[serde(default)]
    pub exp_year: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub brand: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub custom_fields: Option<Vec<CustomFieldInput>>,
}

/// The thirteen vault operations. Every method parks a hold and awaits the
/// reader's answer; the mock in tests implements the same contract.
#[async_trait]
pub trait VaultFacade: Send + Sync {
    async fn search(
        &self,
        caller: &SessionSlot,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Value, VaultToolError>;
    async fn get(
        &self,
        caller: &SessionSlot,
        id: &str,
        reveal: Option<&str>,
    ) -> Result<Value, VaultToolError>;
    async fn create(
        &self,
        caller: &SessionSlot,
        input: &CreateInput,
    ) -> Result<Value, VaultToolError>;
    async fn update(
        &self,
        caller: &SessionSlot,
        id: &str,
        patch: &PatchInput,
    ) -> Result<Value, VaultToolError>;
    async fn trash(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError>;
    async fn restore(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError>;
    async fn totp_code(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError>;
    async fn attachment_get(
        &self,
        caller: &SessionSlot,
        item_id: &str,
        attachment_id: &str,
        destination: &str,
    ) -> Result<Value, VaultToolError>;
    async fn attachment_put(
        &self,
        caller: &SessionSlot,
        item_id: &str,
        path: &str,
        name: Option<&str>,
        replace_attachment_id: Option<&str>,
    ) -> Result<Value, VaultToolError>;
    async fn folder_list(&self, caller: &SessionSlot) -> Result<Value, VaultToolError>;
    async fn folder_create(
        &self,
        caller: &SessionSlot,
        name: &str,
    ) -> Result<Value, VaultToolError>;
    async fn folder_update(
        &self,
        caller: &SessionSlot,
        id: &str,
        name: &str,
    ) -> Result<Value, VaultToolError>;
    async fn folder_delete(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError>;
}

/// The production facade: a weak workspace, so it can read the current
/// `[vault]` session after a reload rather than holding a stale one.
pub struct ProdVaultFacade {
    workspace: Weak<Workspace>,
}

impl ProdVaultFacade {
    pub fn from_arc(workspace: &Arc<Workspace>) -> Self {
        Self { workspace: Arc::downgrade(workspace) }
    }

    pub fn into_arc(self) -> Arc<dyn VaultFacade> {
        Arc::new(self)
    }

    fn workspace(&self) -> Result<Arc<Workspace>, VaultToolError> {
        self.workspace.upgrade().ok_or(VaultToolError::NotConfigured)
    }

    fn session(&self) -> Result<Arc<VaultSession>, VaultToolError> {
        self.workspace()?.vault.read().clone().ok_or(VaultToolError::NotConfigured)
    }

    /// Park the draft, await one answer, settle it, and unlock the shared
    /// session when the card asked for the password. Decline and send-back
    /// are audited here and returned as the tool's refusal; an approval
    /// returns the session and its keys, and the caller audits the outcome.
    async fn gate(
        &self,
        caller: &SessionSlot,
        draft: VaultDraft,
        tool: VaultTool,
        field: Option<&str>,
    ) -> Result<(Arc<VaultSession>, Arc<Unlocked>), VaultToolError> {
        let workspace = self.workspace()?;
        let session = self.session()?;
        let (id, receiver, answerable) =
            workspace.register_vault_ask(caller, draft.clone(), session.is_locked());
        if !answerable {
            return Err(VaultToolError::NoClient);
        }
        let mut guard = SettleOnDrop {
            workspace: Arc::clone(&workspace),
            id,
            caller: caller.clone(),
            armed: true,
        };
        let Ok(mut answer) = receiver.await else {
            return Err(VaultToolError::Abandoned);
        };
        let ending = match &answer.decision {
            VaultAskDecision::Approve => VaultAskEnding::Approved,
            VaultAskDecision::SendBack { .. } => VaultAskEnding::SentBack,
            VaultAskDecision::Decline => VaultAskEnding::Declined,
        };
        guard.armed = false;
        workspace.settle_vault_ask(id, caller, ending);

        let item_id = draft.item_id.as_deref();
        let item_name = draft.item_name.as_deref();
        match &answer.decision {
            VaultAskDecision::Decline => {
                session.audit(caller, tool, item_id, item_name, field, "decline", "not run");
                return Err(VaultToolError::Rejected);
            }
            VaultAskDecision::SendBack { note } => {
                session.audit(caller, tool, item_id, item_name, field, "send_back", "not run");
                return Err(VaultToolError::SentBack { note: note.clone() });
            }
            VaultAskDecision::Approve => {}
        }

        if session.is_locked() {
            let password = answer
                .master_password
                .as_deref()
                .filter(|password| !password.is_empty())
                .ok_or(VaultToolError::Locked)?;
            let two_factor = answer.two_factor.as_deref().filter(|code| !code.is_empty());
            let unlocked = session.unlock(password, two_factor).await;
            if let Some(mut password) = answer.master_password.take() {
                password.zeroize();
            }
            if let Some(mut code) = answer.two_factor.take() {
                code.zeroize();
            }
            if let Err(error) = unlocked {
                session.audit(
                    caller,
                    tool,
                    item_id,
                    item_name,
                    field,
                    "approve",
                    &format!("failed: {error}"),
                );
                return Err(VaultToolError::Vault(error));
            }
        } else {
            if let Some(mut password) = answer.master_password.take() {
                password.zeroize();
            }
            if let Some(mut code) = answer.two_factor.take() {
                code.zeroize();
            }
        }

        let keys = session.keys().map_err(|error| {
            session.audit(
                caller,
                tool,
                item_id,
                item_name,
                field,
                "approve",
                &format!("failed: {error}"),
            );
            VaultToolError::Vault(error)
        })?;
        session.audit(caller, tool, item_id, item_name, field, "approve", "approved");
        Ok((session, keys))
    }

    /// Gate one item operation. When the session is unlocked the item is
    /// fetched first so the card can name it; when it is locked the card is
    /// parked with the id alone, because a token call would fail before the
    /// reader ever saw the password field. The item is fetched after the
    /// unlock either way, so the returned cipher is always current.
    async fn item_gate<F>(
        &self,
        caller: &SessionSlot,
        tool: VaultTool,
        id: &str,
        summary: F,
    ) -> Result<(Arc<VaultSession>, Arc<Unlocked>, Value, VaultDraft), VaultToolError>
    where
        F: FnOnce(Option<&str>, &mut VaultDraft) -> String,
    {
        let session = self.session()?;
        let (cipher, name) = if session.is_locked() {
            (None, None)
        } else {
            let cipher = session.get_cipher(id).await?;
            let keys = session.keys()?;
            let name = decrypt_name(&keys, &cipher).ok();
            (Some(cipher), name)
        };
        let mut draft = VaultDraft::new(tool, String::new());
        draft.item_id = Some(id.to_owned());
        draft.item_name = name;
        let item_name = draft.item_name.clone();
        draft.summary = summary(item_name.as_deref(), &mut draft);
        let (session, keys) = self.gate(caller, draft.clone(), tool, None).await?;
        let cipher = match cipher {
            Some(cipher) => cipher,
            None => session.get_cipher(id).await?,
        };
        Ok((session, keys, cipher, draft))
    }
}

/// Settle a held call as abandoned when the blocked tool future is dropped
/// before it answered.
struct SettleOnDrop {
    workspace: Arc<Workspace>,
    id: uuid::Uuid,
    caller: SessionSlot,
    armed: bool,
}

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.workspace.settle_vault_ask(self.id, &self.caller, VaultAskEnding::Abandoned);
        }
    }
}

#[async_trait]
impl VaultFacade for ProdVaultFacade {
    async fn search(
        &self,
        caller: &SessionSlot,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Value, VaultToolError> {
        let session = self.session()?;
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let (items, hits) = if session.is_locked() {
            (None, None)
        } else {
            let sync = session.sync().await?;
            let keys = session.keys()?;
            let items = search_items(&keys, &sync, query, limit);
            let hits = items.len();
            (Some(items), Some(hits))
        };
        let draft = VaultDraft::new(
            VaultTool::Search,
            match hits {
                Some(hits) => {
                    format!("search for '{query}' ({hits} hit{})", if hits == 1 { "" } else { "s" })
                }
                None => format!("search for '{query}'"),
            },
        );
        let (session, _keys) = self.gate(caller, draft.clone(), VaultTool::Search, None).await?;
        let items = if let Some(items) = items {
            items
        } else {
            let sync = session.sync().await?;
            let keys = session.keys()?;
            search_items(&keys, &sync, query, limit)
        };
        Ok(json!({ "items": items }))
    }

    async fn get(
        &self,
        caller: &SessionSlot,
        id: &str,
        reveal: Option<&str>,
    ) -> Result<Value, VaultToolError> {
        validate_reveal(reveal)?;
        let (_session, keys, cipher, _draft) = self
            .item_gate(caller, VaultTool::Get, id, |name, draft| {
                draft.field = reveal.map(str::to_owned);
                match (name, reveal) {
                    (Some(name), Some(field)) => format!("read {field} for '{name}'"),
                    (Some(name), None) => format!("read metadata for '{name}'"),
                    (None, Some(field)) => format!("read {field} for item {id}"),
                    (None, None) => format!("read metadata for item {id}"),
                }
            })
            .await?;
        let mut result = item_metadata(&keys, &cipher)?;
        if let Some(reveal) = reveal {
            let value = reveal_value(&keys, &cipher, reveal)?;
            result["revealed"] = json!({ "field": reveal, "value": value });
        }
        Ok(result)
    }

    async fn create(
        &self,
        caller: &SessionSlot,
        input: &CreateInput,
    ) -> Result<Value, VaultToolError> {
        let kind = input.kind.as_deref().unwrap_or("login");
        if !matches!(kind, "login" | "note" | "card") {
            return Err(VaultToolError::Message(format!(
                "unknown item type '{kind}'; the types are login, note, card"
            )));
        }
        let mut draft = VaultDraft::new(
            VaultTool::Create,
            match kind {
                "card" => "create a new card".to_owned(),
                "note" => format!("create a secure note '{}'", input.name),
                _ => format!("create a login '{}'", input.name),
            },
        );
        draft.item_name = Some(input.name.clone());
        draft.masked_diff = Some(masked_create_diff(input, kind));
        let (session, keys) = self.gate(caller, draft.clone(), VaultTool::Create, None).await?;
        let body = build_new_cipher(&keys, input, kind)?;
        let created = session.create_cipher(&body).await?;
        let id = created.get("id").and_then(Value::as_str).ok_or_else(|| {
            VaultToolError::Message("the vault did not return the new item's id".to_owned())
        })?;
        let name = created.get("name").and_then(Value::as_str).map_or_else(
            || input.name.clone(),
            |enc| keys.decrypt_item_str(&created, enc).unwrap_or_else(|_| input.name.clone()),
        );
        Ok(json!({ "id": id, "name": name, "type": kind }))
    }

    async fn update(
        &self,
        caller: &SessionSlot,
        id: &str,
        patch: &PatchInput,
    ) -> Result<Value, VaultToolError> {
        let changed = patch_fields(patch);
        if changed.is_empty() {
            return Err(VaultToolError::Message("the patch changes no fields".to_owned()));
        }
        let (session, keys, cipher, draft) = self
            .item_gate(caller, VaultTool::Update, id, |name, draft| {
                draft.masked_diff = Some(masked_update_diff(&changed));
                format!("change {} on '{}'", changed.join(", "), name.unwrap_or(id))
            })
            .await?;
        let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
        let body = apply_patch(&keys, cipher, patch)?;
        session.update_cipher(id, &body).await?;
        Ok(json!({ "id": id, "name": name, "updated": true }))
    }

    async fn trash(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError> {
        let (session, _keys, _cipher, draft) = self
            .item_gate(caller, VaultTool::Trash, id, |name, _draft| {
                format!("move '{}' to the trash", name.unwrap_or(id))
            })
            .await?;
        session.trash_cipher(id).await?;
        let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
        Ok(json!({ "id": id, "name": name, "trashed": true }))
    }

    async fn restore(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError> {
        let (session, _keys, _cipher, draft) = self
            .item_gate(caller, VaultTool::Restore, id, |name, _draft| {
                format!("restore '{}' from the trash", name.unwrap_or(id))
            })
            .await?;
        session.restore_cipher(id).await?;
        let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
        Ok(json!({ "id": id, "name": name, "restored": true }))
    }

    async fn totp_code(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError> {
        let (_session, keys, cipher, draft) = self
            .item_gate(caller, VaultTool::TotpCode, id, |name, _draft| {
                format!("compute a TOTP code for '{}'", name.unwrap_or(id))
            })
            .await?;
        let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
        let seed = cipher
            .pointer("/login/totp")
            .and_then(Value::as_str)
            .ok_or_else(|| VaultToolError::Message(format!("'{name}' has no TOTP seed")))?;
        let seed = keys.decrypt_item_str(&cipher, seed)?;
        let totp =
            forge_connectors::vault::crypto::totp_from_uri(&seed, std::time::SystemTime::now())?;
        Ok(json!({ "code": totp.code, "period": totp.period, "remaining": totp.remaining }))
    }

    async fn attachment_get(
        &self,
        caller: &SessionSlot,
        item_id: &str,
        attachment_id: &str,
        destination: &str,
    ) -> Result<Value, VaultToolError> {
        let session = self.session()?;
        let (cipher, name, file_name, size) = if session.is_locked() {
            (None, None, None, None)
        } else {
            let cipher = session.get_cipher(item_id).await?;
            let keys = session.keys()?;
            let name = decrypt_name(&keys, &cipher)?;
            let (file_name, size) = attachment_named(&keys, &cipher, attachment_id)?;
            (Some(cipher), Some(name), Some(file_name), Some(size))
        };
        let mut draft = VaultDraft::new(
            VaultTool::AttachmentGet,
            match (&name, &file_name) {
                (Some(name), Some(file_name)) => format!("download '{file_name}' from '{name}'"),
                _ => format!("download attachment {attachment_id} from item {item_id}"),
            },
        );
        draft.item_id = Some(item_id.to_owned());
        draft.item_name = name;
        if let (Some(file_name), Some(size)) = (&file_name, size) {
            draft.attachment =
                Some(VaultAttachmentDraft { name: file_name.clone(), size, replaces: None });
        }
        let (session, keys) =
            self.gate(caller, draft.clone(), VaultTool::AttachmentGet, None).await?;
        let cipher = match cipher {
            Some(cipher) => cipher,
            None => session.get_cipher(item_id).await?,
        };
        let meta = session.get_attachment_meta(item_id, attachment_id).await?;
        let url = meta.url.clone().ok_or_else(|| {
            VaultToolError::Message("the vault did not return a download URL".to_owned())
        })?;
        let wrapped = meta.key.clone().ok_or_else(|| {
            VaultToolError::Message("the vault did not return the attachment key".to_owned())
        })?;
        let blob = session.download_attachment(&url).await?;
        let plain = keys.open_attachment(&cipher, &wrapped, &blob)?;
        let workspace = self.workspace()?;
        let path = resolve_destination(&workspace, caller, destination)?;
        write_private(&path, &plain)?;
        Ok(json!({
            "path": path.display().to_string(),
            "size": plain.len(),
            "sha256": forge_connectors::vault::crypto::sha256_hex(&plain),
        }))
    }

    async fn attachment_put(
        &self,
        caller: &SessionSlot,
        item_id: &str,
        path: &str,
        name: Option<&str>,
        replace_attachment_id: Option<&str>,
    ) -> Result<Value, VaultToolError> {
        let workspace = self.workspace()?;
        let source = resolve_source(&workspace, caller, path)?;
        let data = std::fs::read(&source)?;
        let file_name = name
            .map(str::to_owned)
            .or_else(|| source.file_name().map(|name| name.to_string_lossy().into_owned()))
            .ok_or_else(|| VaultToolError::Message("the path has no file name".to_owned()))?;
        let size = data.len() as u64;
        let (session, keys, cipher, draft) = self
            .item_gate(caller, VaultTool::AttachmentPut, item_id, |name, draft| {
                draft.attachment = Some(VaultAttachmentDraft {
                    name: file_name.clone(),
                    size,
                    replaces: replace_attachment_id.map(str::to_owned),
                });
                match replace_attachment_id {
                    Some(_) => format!(
                        "replace an attachment on '{}' with '{file_name}'",
                        name.unwrap_or(item_id)
                    ),
                    None => {
                        format!("upload '{file_name}' to '{}'", name.unwrap_or(item_id))
                    }
                }
            })
            .await?;
        let (wrapped, encrypted_name, blob) =
            keys.prepare_attachment(&cipher, &file_name, &data)?;
        let slot_body = json!({
            "key": wrapped,
            "fileName": encrypted_name,
            "fileSize": blob.len() as u64,
            "adminRequest": false,
        });
        let slot = session.create_attachment_slot(item_id, &slot_body).await?;
        let attachment_id = slot.get("attachmentId").and_then(Value::as_str).ok_or_else(|| {
            VaultToolError::Message("the vault did not return the new attachment id".to_owned())
        })?;
        session.upload_attachment(item_id, attachment_id, blob.clone()).await?;
        let meta = session.get_attachment_meta(item_id, attachment_id).await?;
        if meta.size.is_some_and(|stored| stored != blob.len() as u64) {
            session.audit(
                caller,
                VaultTool::AttachmentPut,
                draft.item_id.as_deref(),
                draft.item_name.as_deref(),
                None,
                "approve",
                "failed: the server stored a different size",
            );
            return Err(VaultToolError::Message(format!(
                "the server stored {} bytes for a {} byte blob",
                meta.size.unwrap_or(0),
                blob.len()
            )));
        }
        if let Some(old) = replace_attachment_id {
            session.delete_attachment(item_id, old).await?;
        }
        Ok(json!({
            "id": attachment_id,
            "name": file_name,
            "size": blob.len(),
            "sha256": forge_connectors::vault::crypto::sha256_hex(&data),
            "replaced": replace_attachment_id,
        }))
    }

    async fn folder_list(&self, caller: &SessionSlot) -> Result<Value, VaultToolError> {
        let draft = VaultDraft::new(VaultTool::FolderList, "list folders");
        let (session, keys) = self.gate(caller, draft.clone(), VaultTool::FolderList, None).await?;
        let folders = session.list_folders().await?;
        let rows: Vec<Value> = folders
            .iter()
            .map(|folder| {
                json!({
                    "id": folder.id,
                    "name": keys.decrypt_user_str(&folder.name).unwrap_or_else(|_| "(unreadable)".to_owned()),
                })
            })
            .collect();
        Ok(json!({ "folders": rows }))
    }

    async fn folder_create(
        &self,
        caller: &SessionSlot,
        name: &str,
    ) -> Result<Value, VaultToolError> {
        let mut draft = VaultDraft::new(VaultTool::FolderCreate, format!("create folder '{name}'"));
        draft.item_name = Some(name.to_owned());
        let (session, keys) =
            self.gate(caller, draft.clone(), VaultTool::FolderCreate, None).await?;
        let encrypted = keys.encrypt_user_str(name)?;
        let created = session.create_folder(&encrypted).await?;
        let id = created.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(json!({ "id": id, "name": name }))
    }

    async fn folder_update(
        &self,
        caller: &SessionSlot,
        id: &str,
        name: &str,
    ) -> Result<Value, VaultToolError> {
        let mut draft =
            VaultDraft::new(VaultTool::FolderUpdate, format!("rename a folder to '{name}'"));
        draft.item_id = Some(id.to_owned());
        let (session, keys) =
            self.gate(caller, draft.clone(), VaultTool::FolderUpdate, None).await?;
        let encrypted = keys.encrypt_user_str(name)?;
        session.update_folder(id, &encrypted).await?;
        Ok(json!({ "id": id, "name": name }))
    }

    async fn folder_delete(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError> {
        let session = self.session()?;
        let name = if session.is_locked() {
            None
        } else {
            let keys = session.keys()?;
            let folders = session.list_folders().await?;
            folders.iter().find(|folder| folder.id == id).map(|folder| {
                keys.decrypt_user_str(&folder.name).unwrap_or_else(|_| "(unreadable)".to_owned())
            })
        };
        let mut draft = VaultDraft::new(
            VaultTool::FolderDelete,
            match &name {
                Some(name) => format!("delete folder '{name}' (its items move to No Folder)"),
                None => format!("delete folder {id} (its items move to No Folder)"),
            },
        );
        draft.item_id = Some(id.to_owned());
        draft.item_name = name;
        let (session, _keys) =
            self.gate(caller, draft.clone(), VaultTool::FolderDelete, None).await?;
        session.delete_folder(id).await?;
        let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
        Ok(json!({ "id": id, "name": name, "deleted": true }))
    }
}

/// The metadata hits for one query: name, username and URIs are decrypted,
/// and nothing else is. Trashed items are skipped, as a vault search does.
fn search_items(keys: &Unlocked, sync: &SyncData, query: &str, limit: usize) -> Vec<Value> {
    let needle = query.trim().to_lowercase();
    let mut items = Vec::new();
    for cipher in &sync.ciphers {
        if cipher.get("deletedDate").and_then(Value::as_str).is_some() {
            continue;
        }
        let name = decrypt_name(keys, cipher).unwrap_or_else(|_| "(unreadable)".to_owned());
        let username = decrypt_username(keys, cipher);
        let uris = decrypt_uris(keys, cipher);
        let hit = needle.is_empty()
            || name.to_lowercase().contains(&needle)
            || username.as_deref().is_some_and(|u| u.to_lowercase().contains(&needle))
            || uris.iter().any(|uri| uri.to_lowercase().contains(&needle));
        if !hit {
            continue;
        }
        let folder = folder_name(keys, sync, cipher);
        items.push(json!({
            "id": cipher.get("id").and_then(Value::as_str),
            "name": name,
            "username": username,
            "uris": uris,
            "folder": folder,
            "collection": Value::Null,
            "updated_at": cipher.get("revisionDate").and_then(Value::as_str),
        }));
        if items.len() >= limit {
            break;
        }
    }
    items
}

fn decrypt_name(keys: &Unlocked, cipher: &Value) -> Result<String, VaultToolError> {
    let enc = cipher
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| VaultToolError::Message("the item has no name".to_owned()))?;
    Ok(keys.decrypt_item_str(cipher, enc)?)
}

fn decrypt_username(keys: &Unlocked, cipher: &Value) -> Option<String> {
    cipher
        .pointer("/login/username")
        .and_then(Value::as_str)
        .and_then(|enc| keys.decrypt_item_str(cipher, enc).ok())
        .filter(|value| !value.is_empty())
}

fn decrypt_uris(keys: &Unlocked, cipher: &Value) -> Vec<String> {
    let mut uris = Vec::new();
    if let Some(list) = cipher.pointer("/login/uris").and_then(Value::as_array) {
        for entry in list {
            if let Some(enc) = entry.get("uri").and_then(Value::as_str)
                && let Ok(uri) = keys.decrypt_item_str(cipher, enc)
            {
                uris.push(uri);
            }
        }
    }
    if uris.is_empty()
        && let Some(enc) = cipher.pointer("/login/uri").and_then(Value::as_str)
        && let Ok(uri) = keys.decrypt_item_str(cipher, enc)
    {
        uris.push(uri);
    }
    uris
}

fn folder_name(keys: &Unlocked, sync: &SyncData, cipher: &Value) -> Option<String> {
    let id = cipher.get("folderId").and_then(Value::as_str)?;
    sync.folders
        .iter()
        .find(|folder| folder.id == id)
        .and_then(|folder| keys.decrypt_user_str(&folder.name).ok())
}

fn item_metadata(keys: &Unlocked, cipher: &Value) -> Result<Value, VaultToolError> {
    let mut metadata = json!({
        "id": cipher.get("id").and_then(Value::as_str),
        "name": decrypt_name(keys, cipher)?,
        "type": cipher.get("type").and_then(Value::as_u64),
        "folder_id": cipher.get("folderId").and_then(Value::as_str),
        "collection_ids": cipher.get("collectionIds").cloned().unwrap_or(Value::Null),
        "favorite": cipher.get("favorite").and_then(Value::as_bool).unwrap_or(false),
        "revision_date": cipher.get("revisionDate").and_then(Value::as_str),
        "deleted_date": cipher.get("deletedDate").and_then(Value::as_str),
        "attachments": attachment_rows(keys, cipher),
    });
    if let Some(username) = decrypt_username(keys, cipher) {
        metadata["username"] = json!(username);
    }
    let uris = decrypt_uris(keys, cipher);
    if !uris.is_empty() {
        metadata["uris"] = json!(uris);
    }
    if let Some(brand) = cipher
        .pointer("/card/brand")
        .and_then(Value::as_str)
        .and_then(|enc| keys.decrypt_item_str(cipher, enc).ok())
    {
        metadata["card_brand"] = json!(brand);
    }
    Ok(metadata)
}

fn attachment_rows(keys: &Unlocked, cipher: &Value) -> Value {
    let Some(list) = cipher.get("attachments").and_then(Value::as_array) else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        list.iter()
            .map(|attachment| {
                let name = attachment
                    .get("fileName")
                    .and_then(Value::as_str)
                    .and_then(|enc| keys.decrypt_item_str(cipher, enc).ok())
                    .unwrap_or_else(|| "(unreadable)".to_owned());
                json!({
                    "id": attachment.get("id").and_then(Value::as_str),
                    "file_name": name,
                    "size": attachment.get("size").and_then(Value::as_u64),
                })
            })
            .collect(),
    )
}

fn attachment_named(
    keys: &Unlocked,
    cipher: &Value,
    attachment_id: &str,
) -> Result<(String, u64), VaultToolError> {
    let list = cipher
        .get("attachments")
        .and_then(Value::as_array)
        .ok_or_else(|| VaultToolError::Message("the item has no attachments".to_owned()))?;
    let attachment = list
        .iter()
        .find(|attachment| attachment.get("id").and_then(Value::as_str) == Some(attachment_id))
        .ok_or_else(|| {
            VaultToolError::Message(format!("the item has no attachment '{attachment_id}'"))
        })?;
    let name = attachment
        .get("fileName")
        .and_then(Value::as_str)
        .map(|enc| keys.decrypt_item_str(cipher, enc))
        .transpose()?
        .unwrap_or_else(|| "(unreadable)".to_owned());
    let size = attachment.get("size").and_then(Value::as_u64).unwrap_or(0);
    Ok((name, size))
}

fn validate_reveal(reveal: Option<&str>) -> Result<(), VaultToolError> {
    let Some(reveal) = reveal else { return Ok(()) };
    let accepted = matches!(
        reveal,
        "password" | "notes" | "card.cardholder" | "card.number" | "card.expiry" | "card.code"
    ) || reveal.starts_with("custom:");
    if accepted {
        Ok(())
    } else {
        Err(VaultToolError::Message(format!(
            "unknown reveal '{reveal}'; use password, notes, custom:<name>, card.cardholder, \
             card.number, card.expiry or card.code"
        )))
    }
}

fn reveal_value(keys: &Unlocked, cipher: &Value, reveal: &str) -> Result<Value, VaultToolError> {
    match reveal {
        "password" => {
            let enc = cipher
                .pointer("/login/password")
                .and_then(Value::as_str)
                .ok_or_else(|| VaultToolError::Message("the item has no password".to_owned()))?;
            Ok(json!(keys.decrypt_item_str(cipher, enc)?))
        }
        "notes" => {
            let enc = cipher
                .get("notes")
                .and_then(Value::as_str)
                .ok_or_else(|| VaultToolError::Message("the item has no notes".to_owned()))?;
            Ok(json!(keys.decrypt_item_str(cipher, enc)?))
        }
        "card.cardholder" => card_field(keys, cipher, "cardholderName"),
        "card.number" => card_field(keys, cipher, "number"),
        "card.code" => card_field(keys, cipher, "code"),
        "card.expiry" => {
            let month = card_field(keys, cipher, "expMonth")?;
            let year = card_field(keys, cipher, "expYear")?;
            Ok(json!({ "expMonth": month, "expYear": year }))
        }
        custom if custom.starts_with("custom:") => {
            let wanted = &custom["custom:".len()..];
            let fields = cipher.get("fields").and_then(Value::as_array).ok_or_else(|| {
                VaultToolError::Message("the item has no custom fields".to_owned())
            })?;
            for field in fields {
                let name = field
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|enc| keys.decrypt_item_str(cipher, enc))
                    .transpose()?;
                if name.as_deref() == Some(wanted)
                    && let Some(enc) = field.get("value").and_then(Value::as_str)
                {
                    return Ok(json!(keys.decrypt_item_str(cipher, enc)?));
                }
            }
            Err(VaultToolError::Message(format!("the item has no custom field '{wanted}'")))
        }
        other => Err(VaultToolError::Message(format!("unknown reveal '{other}'"))),
    }
}

fn card_field(keys: &Unlocked, cipher: &Value, field: &str) -> Result<Value, VaultToolError> {
    let enc = cipher
        .pointer(&format!("/card/{field}"))
        .and_then(Value::as_str)
        .ok_or_else(|| VaultToolError::Message(format!("the item has no card {field}")))?;
    Ok(json!(keys.decrypt_item_str(cipher, enc)?))
}

fn masked_create_diff(input: &CreateInput, kind: &str) -> String {
    match kind {
        "card" => "new card: number, expiry and CVV are set and never drawn".to_owned(),
        "note" => "secure note".to_owned(),
        _ => {
            let mut fields = vec!["name"];
            if input.username.is_some() {
                fields.push("username");
            }
            if input.password.is_some() {
                fields.push("password");
            }
            if input.uri.is_some() {
                fields.push("uri");
            }
            if input.notes.is_some() {
                fields.push("notes");
            }
            if input.custom_fields.is_some() {
                fields.push("custom fields");
            }
            format!("new login: {}", fields.join(", "))
        }
    }
}

fn patch_fields(patch: &PatchInput) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if patch.name.is_some() {
        fields.push("name");
    }
    if patch.username.is_some() {
        fields.push("username");
    }
    if patch.password.is_some() {
        fields.push("password");
    }
    if patch.uri.is_some() {
        fields.push("uri");
    }
    if patch.notes.is_some() {
        fields.push("notes");
    }
    if patch.cardholder_name.is_some()
        || patch.number.is_some()
        || patch.exp_month.is_some()
        || patch.exp_year.is_some()
        || patch.code.is_some()
        || patch.brand.is_some()
    {
        fields.push("card details");
    }
    if patch.folder_id.is_some() {
        fields.push("folder");
    }
    if patch.custom_fields.is_some() {
        fields.push("custom fields");
    }
    fields
}

fn masked_update_diff(changed: &[&str]) -> String {
    if changed.contains(&"password") {
        let mut parts: Vec<&str> =
            changed.iter().copied().filter(|field| *field != "password").collect();
        parts.push("password changed");
        return parts.join(", ");
    }
    if changed.contains(&"card details") {
        let mut parts: Vec<&str> =
            changed.iter().copied().filter(|field| *field != "card details").collect();
        parts.push("card details changed");
        return parts.join(", ");
    }
    changed.join(", ")
}

fn build_new_cipher(
    keys: &Unlocked,
    input: &CreateInput,
    kind: &str,
) -> Result<Value, VaultToolError> {
    let base = Value::Null;
    let mut cipher = json!({
        "type": match kind { "card" => 3, "note" => 2, _ => 1 },
        "name": keys.encrypt_item_str(&base, &input.name)?,
        "favorite": false,
        "reprompt": 0,
    });
    if let Some(notes) = &input.notes {
        cipher["notes"] = json!(keys.encrypt_item_str(&base, notes)?);
    }
    if let Some(folder) = &input.folder_id {
        cipher["folderId"] = json!(folder);
    }
    match kind {
        "card" => {
            let mut card = serde_json::Map::new();
            for (field, value) in [
                ("cardholderName", input.cardholder_name.as_ref()),
                ("brand", input.brand.as_ref()),
                ("number", input.number.as_ref()),
                ("expMonth", input.exp_month.as_ref()),
                ("expYear", input.exp_year.as_ref()),
                ("code", input.code.as_ref()),
            ] {
                if let Some(value) = value {
                    card.insert(field.to_owned(), json!(keys.encrypt_item_str(&base, value)?));
                }
            }
            cipher["card"] = Value::Object(card);
        }
        "note" => {
            cipher["secureNote"] = json!({ "type": 0 });
        }
        _ => {
            let mut login = serde_json::Map::new();
            if let Some(username) = &input.username {
                login.insert("username".to_owned(), json!(keys.encrypt_item_str(&base, username)?));
            }
            if let Some(password) = &input.password {
                login.insert("password".to_owned(), json!(keys.encrypt_item_str(&base, password)?));
            }
            if let Some(uri) = &input.uri {
                let encrypted = keys.encrypt_item_str(&base, uri)?;
                login.insert("uri".to_owned(), json!(encrypted));
                login
                    .insert("uris".to_owned(), json!([{ "uri": encrypted, "match": Value::Null }]));
            }
            cipher["login"] = Value::Object(login);
        }
    }
    if let Some(fields) = &input.custom_fields {
        cipher["fields"] = custom_fields(keys, &base, fields)?;
    }
    Ok(cipher)
}

fn custom_fields(
    keys: &Unlocked,
    cipher: &Value,
    fields: &[CustomFieldInput],
) -> Result<Value, VaultToolError> {
    let mut rows = Vec::new();
    for field in fields {
        let kind = match field.kind.as_deref() {
            Some("hidden") => 1,
            Some("boolean") => 2,
            _ => 0,
        };
        rows.push(json!({
            "name": keys.encrypt_item_str(cipher, &field.name)?,
            "value": keys.encrypt_item_str(cipher, &field.value)?,
            "type": kind,
        }));
    }
    Ok(Value::Array(rows))
}

fn apply_patch(
    keys: &Unlocked,
    mut cipher: Value,
    patch: &PatchInput,
) -> Result<Value, VaultToolError> {
    if let Some(name) = &patch.name {
        cipher["name"] = json!(keys.encrypt_item_str(&cipher, name)?);
    }
    if let Some(notes) = &patch.notes {
        cipher["notes"] = json!(keys.encrypt_item_str(&cipher, notes)?);
    }
    if let Some(folder) = &patch.folder_id {
        cipher["folderId"] = json!(folder);
    }
    if patch.username.is_some() || patch.password.is_some() || patch.uri.is_some() {
        if cipher.get("login").is_none() {
            cipher["login"] = json!({});
        }
        if let Some(username) = &patch.username {
            cipher["login"]["username"] = json!(keys.encrypt_item_str(&cipher, username)?);
        }
        if let Some(password) = &patch.password {
            cipher["login"]["password"] = json!(keys.encrypt_item_str(&cipher, password)?);
        }
        if let Some(uri) = &patch.uri {
            let encrypted = keys.encrypt_item_str(&cipher, uri)?;
            cipher["login"]["uri"] = json!(encrypted);
            cipher["login"]["uris"] = json!([{ "uri": encrypted, "match": Value::Null }]);
        }
    }
    let card_fields = [
        ("cardholderName", patch.cardholder_name.as_ref()),
        ("brand", patch.brand.as_ref()),
        ("number", patch.number.as_ref()),
        ("expMonth", patch.exp_month.as_ref()),
        ("expYear", patch.exp_year.as_ref()),
        ("code", patch.code.as_ref()),
    ];
    if card_fields.iter().any(|(_, value)| value.is_some()) {
        if cipher.get("card").is_none() {
            cipher["card"] = json!({});
        }
        for (field, value) in card_fields {
            if let Some(value) = value {
                cipher["card"][field] = json!(keys.encrypt_item_str(&cipher, value)?);
            }
        }
    }
    if let Some(fields) = &patch.custom_fields {
        cipher["fields"] = custom_fields(keys, &cipher, fields)?;
    }
    Ok(cipher)
}

fn resolve_destination(
    workspace: &Workspace,
    caller: &SessionSlot,
    destination: &str,
) -> Result<PathBuf, VaultToolError> {
    let cwd = workspace.cwd_for_session(caller).ok_or_else(|| {
        VaultToolError::Message("the session has no working directory".to_owned())
    })?;
    let cwd = std::fs::canonicalize(cwd)
        .map_err(|error| VaultToolError::Io(format!("the working directory: {error}")))?;
    let target = Path::new(destination);
    let absolute = if target.is_absolute() { target.to_path_buf() } else { cwd.join(target) };
    let parent = absolute.parent().ok_or_else(|| {
        VaultToolError::Message("the destination has no parent directory".to_owned())
    })?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| VaultToolError::Io(format!("the destination directory: {error}")))?;
    if !parent.starts_with(&cwd) {
        return Err(VaultToolError::Message(
            "the destination is outside this session's working directory".to_owned(),
        ));
    }
    let name = absolute
        .file_name()
        .ok_or_else(|| VaultToolError::Message("the destination has no file name".to_owned()))?;
    Ok(parent.join(name))
}

fn resolve_source(
    workspace: &Workspace,
    caller: &SessionSlot,
    path: &str,
) -> Result<PathBuf, VaultToolError> {
    let cwd = workspace.cwd_for_session(caller).ok_or_else(|| {
        VaultToolError::Message("the session has no working directory".to_owned())
    })?;
    let cwd = std::fs::canonicalize(cwd)
        .map_err(|error| VaultToolError::Io(format!("the working directory: {error}")))?;
    let target = Path::new(path);
    let absolute = if target.is_absolute() { target.to_path_buf() } else { cwd.join(target) };
    let source = std::fs::canonicalize(&absolute)
        .map_err(|error| VaultToolError::Io(format!("the attachment file: {error}")))?;
    if !source.starts_with(&cwd) {
        return Err(VaultToolError::Message(
            "the attachment file is outside this session's working directory".to_owned(),
        ));
    }
    Ok(source)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), VaultToolError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// A mock facade for the tool tests: records each call and returns a fixed
/// JSON acknowledgment. The error path can be armed for one call.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockVaultFacade {
    calls: parking_lot::Mutex<Vec<String>>,
    failure: parking_lot::Mutex<Option<String>>,
}

#[cfg(test)]
impl MockVaultFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn into_arc(self) -> Arc<dyn VaultFacade> {
        Arc::new(self)
    }

    pub(crate) fn fail_next(&self, message: impl Into<String>) {
        *self.failure.lock() = Some(message.into());
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }

    fn record(&self, tool: VaultTool) -> Result<Value, VaultToolError> {
        self.calls.lock().push(tool.as_str().to_owned());
        if let Some(message) = self.failure.lock().take() {
            return Err(VaultToolError::Message(message));
        }
        Ok(json!({ "tool": tool.as_str() }))
    }
}

#[cfg(test)]
#[async_trait]
impl VaultFacade for MockVaultFacade {
    async fn search(
        &self,
        _caller: &SessionSlot,
        _query: &str,
        _limit: Option<usize>,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::Search)
    }
    async fn get(
        &self,
        _caller: &SessionSlot,
        _id: &str,
        _reveal: Option<&str>,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::Get)
    }
    async fn create(
        &self,
        _caller: &SessionSlot,
        _input: &CreateInput,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::Create)
    }
    async fn update(
        &self,
        _caller: &SessionSlot,
        _id: &str,
        _patch: &PatchInput,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::Update)
    }
    async fn trash(&self, _caller: &SessionSlot, _id: &str) -> Result<Value, VaultToolError> {
        self.record(VaultTool::Trash)
    }
    async fn restore(&self, _caller: &SessionSlot, _id: &str) -> Result<Value, VaultToolError> {
        self.record(VaultTool::Restore)
    }
    async fn totp_code(&self, _caller: &SessionSlot, _id: &str) -> Result<Value, VaultToolError> {
        self.record(VaultTool::TotpCode)
    }
    async fn attachment_get(
        &self,
        _caller: &SessionSlot,
        _item_id: &str,
        _attachment_id: &str,
        _destination: &str,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::AttachmentGet)
    }
    async fn attachment_put(
        &self,
        _caller: &SessionSlot,
        _item_id: &str,
        _path: &str,
        _name: Option<&str>,
        _replace_attachment_id: Option<&str>,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::AttachmentPut)
    }
    async fn folder_list(&self, _caller: &SessionSlot) -> Result<Value, VaultToolError> {
        self.record(VaultTool::FolderList)
    }
    async fn folder_create(
        &self,
        _caller: &SessionSlot,
        _name: &str,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::FolderCreate)
    }
    async fn folder_update(
        &self,
        _caller: &SessionSlot,
        _id: &str,
        _name: &str,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::FolderUpdate)
    }
    async fn folder_delete(
        &self,
        _caller: &SessionSlot,
        _id: &str,
    ) -> Result<Value, VaultToolError> {
        self.record(VaultTool::FolderDelete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VaultConfig;
    use crate::protocol::{Command, PendingAsk};
    use forge_primitives::vault::{VaultAskAnswer, VaultAskDecision};

    fn locked_workspace() -> (
        Arc<Workspace>,
        Arc<ProdVaultFacade>,
        SessionSlot,
        tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
    ) {
        let (workspace, rx) = Workspace::testing_stub();
        let config = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "hi@vedhavyas.dev".to_owned(),
        };
        let session =
            Arc::new(VaultSession::new(&config, None, Path::new("/tmp")).expect("a session"));
        *workspace.vault.write() = Some(session);
        let facade = Arc::new(ProdVaultFacade::from_arc(&workspace));
        (workspace, facade, SessionSlot::lead("Default", "forge"), rx)
    }

    async fn parked_draft(workspace: &Workspace, caller: &SessionSlot) -> VaultDraft {
        for _ in 0..200 {
            if let Some(PendingAsk::Vault(held)) = workspace
                .pending_asks(caller)
                .into_iter()
                .find(|ask| matches!(ask, PendingAsk::Vault(_)))
            {
                return *held;
            }
            tokio::task::yield_now().await;
        }
        panic!("the locked call parks a card");
    }

    fn decline(workspace: &Arc<Workspace>, caller: &SessionSlot, id: uuid::Uuid) {
        workspace
            .dispatch(Command::RespondVaultAsk {
                key: caller.clone(),
                id,
                answer: VaultAskAnswer {
                    decision: VaultAskDecision::Decline,
                    master_password: None,
                    two_factor: None,
                },
            })
            .expect("the answer routes");
    }

    /// A locked session must park the card asking for the password before it
    /// makes any token call. The first call after a forge start is the case:
    /// an error returned before the hold would leave the reader no way to
    /// unlock.
    #[tokio::test]
    async fn a_locked_get_parks_the_unlock_card_before_any_token_call() {
        let (workspace, facade, caller, _rx) = locked_workspace();
        let asked = caller.clone();
        let task = tokio::spawn(async move { facade.get(&asked, "item-1", None).await });

        let draft = parked_draft(&workspace, &caller).await;
        assert!(draft.needs_unlock, "the card asks for the master password");
        assert_eq!(draft.item_id.as_deref(), Some("item-1"));
        assert_eq!(draft.summary, "read metadata for item item-1");

        decline(&workspace, &caller, draft.id);
        assert!(
            matches!(task.await.expect("the call finishes"), Err(VaultToolError::Rejected)),
            "a decline runs nothing"
        );
    }

    /// A locked search parks the query rather than failing on the sync.
    #[tokio::test]
    async fn a_locked_search_parks_the_query() {
        let (workspace, facade, caller, _rx) = locked_workspace();
        let asked = caller.clone();
        let task = tokio::spawn(async move { facade.search(&asked, "bank", None).await });

        let draft = parked_draft(&workspace, &caller).await;
        assert!(draft.needs_unlock);
        assert!(draft.summary.contains("search for 'bank'"), "{}", draft.summary);

        decline(&workspace, &caller, draft.id);
        assert!(matches!(task.await.expect("the call finishes"), Err(VaultToolError::Rejected)));
    }
}
