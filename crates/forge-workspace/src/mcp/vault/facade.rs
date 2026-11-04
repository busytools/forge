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

/// The server's attachment ceiling, mirrored so forge refuses before a read
/// or write buffers more than the vault accepts.
const MAX_ATTACHMENT_BYTES: u64 = 525 * 1024 * 1024;

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
#[derive(Clone, Deserialize)]
pub struct CustomFieldInput {
    pub name: String,
    pub value: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// Hand-written because a custom field's value is a secret.
impl std::fmt::Debug for CustomFieldInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomFieldInput")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .field("kind", &self.kind)
            .finish()
    }
}

/// The arguments `vault_create` accepts.
#[derive(Clone, Default, Deserialize)]
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

/// Hand-written because its password, card number, CVV and custom values are
/// secrets.
impl std::fmt::Debug for CreateInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateInput")
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .field("uri", &self.uri)
            .field("notes", &self.notes.as_ref().map(|_| "[redacted]"))
            .field("cardholder_name", &self.cardholder_name)
            .field("number", &self.number.as_ref().map(|_| "[redacted]"))
            .field("exp_month", &self.exp_month)
            .field("exp_year", &self.exp_year)
            .field("code", &self.code.as_ref().map(|_| "[redacted]"))
            .field("brand", &self.brand)
            .field("folder_id", &self.folder_id)
            .field("custom_fields", &self.custom_fields)
            .finish()
    }
}

/// The fields `vault_update` may change.
#[derive(Clone, Default, Deserialize)]
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

/// Hand-written because its password, card number, CVV and custom values are
/// secrets.
impl std::fmt::Debug for PatchInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatchInput")
            .field("name", &self.name)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .field("uri", &self.uri)
            .field("notes", &self.notes.as_ref().map(|_| "[redacted]"))
            .field("cardholder_name", &self.cardholder_name)
            .field("number", &self.number.as_ref().map(|_| "[redacted]"))
            .field("exp_month", &self.exp_month)
            .field("exp_year", &self.exp_year)
            .field("code", &self.code.as_ref().map(|_| "[redacted]"))
            .field("brand", &self.brand)
            .field("folder_id", &self.folder_id)
            .field("custom_fields", &self.custom_fields)
            .finish()
    }
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
    ) -> Result<Approved, VaultToolError> {
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
        // A call that names a field on its draft (a reveal) must audit it even
        // when the gate's own argument is empty.
        let field = field.or(draft.field.as_deref());
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

        // A `[vault]` reload builds a new session. The answer was drawn against
        // the old account, so running it would send the old account's password
        // hash to the old URL - or spend a password on a session that is gone.
        let current = self.session();
        let reloaded = match current {
            Ok(current) => !Arc::ptr_eq(&current, &session),
            Err(_) => true,
        };
        if reloaded {
            if let Some(mut password) = answer.master_password.take() {
                password.zeroize();
            }
            if let Some(mut code) = answer.two_factor.take() {
                code.zeroize();
            }
            session.audit(
                caller,
                tool,
                item_id,
                item_name,
                field,
                "approve",
                "failed: the vault configuration changed",
            );
            return Err(VaultToolError::Message(
                "the vault configuration changed while the call was parked; nothing ran".to_owned(),
            ));
        }

        if session.is_locked() {
            let Some(password) =
                answer.master_password.as_deref().filter(|password| !password.is_empty())
            else {
                session.audit(
                    caller,
                    tool,
                    item_id,
                    item_name,
                    field,
                    "approve",
                    "failed: a locked session was approved without the master password",
                );
                return Err(VaultToolError::Locked);
            };
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
            // One unlock serves every card parked behind this one: each is an
            // ordinary approve card now, and a client that already drew it
            // merges the restamped frame by id.
            workspace.rebase_vault_asks(false);
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
        Ok(Approved {
            workspace,
            session,
            keys,
            caller: caller.clone(),
            tool,
            item_id: item_id.map(str::to_owned),
            item_name: item_name.map(str::to_owned),
            field: field.map(str::to_owned),
        })
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
    ) -> Result<(Approved, Value, VaultDraft), VaultToolError>
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
        let approved = self.gate(caller, draft.clone(), tool, None).await?;
        // A reload between the pre-gate read and the gate's own read leaves
        // `cipher` from the old account; the approval names it, so it must not
        // be read by the new session.
        if !Arc::ptr_eq(&approved.session, &session) {
            return approved.fail(VaultToolError::Message(
                "the vault configuration changed while the call was parked; nothing ran".to_owned(),
            ));
        }
        let cipher = match cipher {
            Some(cipher) => cipher,
            None => match approved.session.get_cipher(id).await {
                Ok(cipher) => cipher,
                Err(error) => return approved.fail(VaultToolError::Vault(error)),
            },
        };
        Ok((approved, cipher, draft))
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

/// An approved call: the session and keys it runs with, plus the facts its
/// audit line records. [`Approved::done`] writes that line from the
/// operation's own result, never before it has run.
struct Approved {
    workspace: Arc<Workspace>,
    session: Arc<VaultSession>,
    keys: Arc<Unlocked>,
    caller: SessionSlot,
    tool: VaultTool,
    item_id: Option<String>,
    item_name: Option<String>,
    field: Option<String>,
}

impl Approved {
    fn done<T>(&self, result: Result<T, VaultToolError>) -> Result<T, VaultToolError> {
        // A decryption failure under a key that has not changed means the
        // account rotated its keys; the stale ones are dropped so the next
        // held call asks for the master password.
        if matches!(
            &result,
            Err(VaultToolError::Vault(VaultError::Crypto(_)) | VaultToolError::Crypto(_))
        ) {
            self.session.note_decrypt_failure();
        }
        // A call's own 401 path may have locked the session (a failed refresh),
        // so every card still parked asks for the password again.
        if self.session.is_locked() {
            self.workspace.rebase_vault_asks(true);
        }
        let outcome = match &result {
            Ok(_) => "ok".to_owned(),
            Err(error) => format!("failed: {error}"),
        };
        self.session.audit(
            &self.caller,
            self.tool,
            self.item_id.as_deref(),
            self.item_name.as_deref(),
            self.field.as_deref(),
            "approve",
            &outcome,
        );
        result
    }

    /// Settle a failure that happened after the gate without running the
    /// operation: the audit line is written from here, never skipped.
    fn fail<T>(&self, error: VaultToolError) -> Result<T, VaultToolError> {
        self.done(Err(error))
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
        let approved = self.gate(caller, draft.clone(), VaultTool::Search, None).await?;
        let session = Arc::clone(&approved.session);
        let result = async move {
            let items = if let Some(items) = items {
                items
            } else {
                let sync = session.sync().await?;
                let keys = session.keys()?;
                search_items(&keys, &sync, query, limit)
            };
            Ok(json!({ "items": items }))
        }
        .await;
        approved.done(result)
    }

    async fn get(
        &self,
        caller: &SessionSlot,
        id: &str,
        reveal: Option<&str>,
    ) -> Result<Value, VaultToolError> {
        validate_reveal(reveal)?;
        let (approved, cipher, _draft) = self
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
        let keys = Arc::clone(&approved.keys);
        let result = async move {
            let mut result = item_metadata(&keys, &cipher)?;
            if let Some(reveal) = reveal {
                let value = reveal_value(&keys, &cipher, reveal)?;
                result["revealed"] = json!({ "field": reveal, "value": value });
            }
            Ok(result)
        }
        .await;
        approved.done(result)
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
        let approved = self.gate(caller, draft.clone(), VaultTool::Create, None).await?;
        let session = Arc::clone(&approved.session);
        let keys = Arc::clone(&approved.keys);
        let result = async move {
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
        .await;
        approved.done(result)
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
        let (approved, cipher, draft) = self
            .item_gate(caller, VaultTool::Update, id, |name, draft| {
                draft.masked_diff = Some(masked_update_diff(&changed));
                format!("change {} on '{}'", changed.join(", "), name.unwrap_or(id))
            })
            .await?;
        let session = Arc::clone(&approved.session);
        let keys = Arc::clone(&approved.keys);
        let result = async move {
            let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
            let body = apply_patch(&keys, cipher, patch)?;
            session.update_cipher(id, &body).await?;
            Ok(json!({ "id": id, "name": name, "updated": true }))
        }
        .await;
        approved.done(result)
    }

    async fn trash(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError> {
        let (approved, _cipher, draft) = self
            .item_gate(caller, VaultTool::Trash, id, |name, _draft| {
                format!("move '{}' to the trash", name.unwrap_or(id))
            })
            .await?;
        let session = Arc::clone(&approved.session);
        let result = async move {
            session.trash_cipher(id).await?;
            let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
            Ok(json!({ "id": id, "name": name, "trashed": true }))
        }
        .await;
        approved.done(result)
    }

    async fn restore(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError> {
        let (approved, _cipher, draft) = self
            .item_gate(caller, VaultTool::Restore, id, |name, _draft| {
                format!("restore '{}' from the trash", name.unwrap_or(id))
            })
            .await?;
        let session = Arc::clone(&approved.session);
        let result = async move {
            session.restore_cipher(id).await?;
            let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
            Ok(json!({ "id": id, "name": name, "restored": true }))
        }
        .await;
        approved.done(result)
    }

    async fn totp_code(&self, caller: &SessionSlot, id: &str) -> Result<Value, VaultToolError> {
        let (approved, cipher, draft) = self
            .item_gate(caller, VaultTool::TotpCode, id, |name, _draft| {
                format!("compute a TOTP code for '{}'", name.unwrap_or(id))
            })
            .await?;
        let keys = Arc::clone(&approved.keys);
        let result = async move {
            let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
            let seed = cipher
                .pointer("/login/totp")
                .and_then(Value::as_str)
                .ok_or_else(|| VaultToolError::Message(format!("'{name}' has no TOTP seed")))?;
            let seed = keys.decrypt_item_str(&cipher, seed)?;
            let totp = forge_connectors::vault::crypto::totp_from_uri(
                &seed,
                std::time::SystemTime::now(),
            )?;
            Ok(json!({ "code": totp.code, "period": totp.period, "remaining": totp.remaining }))
        }
        .await;
        approved.done(result)
    }

    async fn attachment_get(
        &self,
        caller: &SessionSlot,
        item_id: &str,
        attachment_id: &str,
        destination: &str,
    ) -> Result<Value, VaultToolError> {
        let session = self.session()?;
        let workspace = self.workspace()?;
        // The destination is resolved before the card parks: it needs no keys,
        // and the reader must see where the file would land even on a locked
        // card that cannot name the item.
        let path = resolve_destination(&workspace, caller, destination)?;
        let (cipher, name, file_name, size) = if session.is_locked() {
            (None, None, None, None)
        } else {
            let cipher = session.get_cipher(item_id).await?;
            let keys = session.keys()?;
            let name =
                decrypt_name(&keys, &cipher).map_err(|error| decrypt_failed(&session, error))?;
            let (file_name, size) = attachment_named(&keys, &cipher, attachment_id)
                .map_err(|error| decrypt_failed(&session, error))?;
            (Some(cipher), Some(name), Some(file_name), Some(size))
        };
        let destination_shown = path.display().to_string();
        let mut draft = VaultDraft::new(
            VaultTool::AttachmentGet,
            match (&name, &file_name) {
                (Some(name), Some(file_name)) => {
                    format!("download '{file_name}' from '{name}' to {destination_shown}")
                }
                _ => format!(
                    "download attachment {attachment_id} from item {item_id} to {destination_shown}"
                ),
            },
        );
        draft.item_id = Some(item_id.to_owned());
        draft.item_name = name;
        if let (Some(file_name), Some(size)) = (&file_name, size) {
            draft.attachment =
                Some(VaultAttachmentDraft { name: file_name.clone(), size, replaces: None });
        }
        let approved = self.gate(caller, draft.clone(), VaultTool::AttachmentGet, None).await?;
        let result = async {
            let cipher = match cipher {
                Some(cipher) => cipher,
                None => approved.session.get_cipher(item_id).await?,
            };
            let meta = approved.session.get_attachment_meta(item_id, attachment_id).await?;
            let url = meta.url.clone().ok_or_else(|| {
                VaultToolError::Message("the vault did not return a download URL".to_owned())
            })?;
            let wrapped = meta.key.clone().ok_or_else(|| {
                VaultToolError::Message("the vault did not return the attachment key".to_owned())
            })?;
            let blob = approved.session.download_attachment(&url, MAX_ATTACHMENT_BYTES).await?;
            let plain = approved.keys.open_attachment(&cipher, &wrapped, &blob)?;
            let size = plain.len();
            let sha256 = forge_connectors::vault::crypto::sha256_hex(&plain);
            let written = path.clone();
            tokio::task::spawn_blocking(move || write_private(&written, &plain))
                .await
                .map_err(|error| VaultToolError::Io(format!("the attachment write: {error}")))??;
            Ok(json!({
                "path": path.display().to_string(),
                "size": size,
                "sha256": sha256,
            }))
        }
        .await;
        approved.done(result)
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
        // The size is checked from metadata before a byte is read, and the
        // read itself runs off the async thread.
        let read_path = source.clone();
        let data =
            tokio::task::spawn_blocking(move || read_attachment(&read_path, MAX_ATTACHMENT_BYTES))
                .await
                .map_err(|error| VaultToolError::Io(format!("the attachment read: {error}")))??;
        let file_name = name
            .map(str::to_owned)
            .or_else(|| source.file_name().map(|name| name.to_string_lossy().into_owned()))
            .ok_or_else(|| VaultToolError::Message("the path has no file name".to_owned()))?;
        let size = data.len() as u64;
        // The card names the file being replaced, which needs the keys. A locked
        // session parks with the id instead, and says only that it replaces one.
        let replaced_name = if let Some(old) = replace_attachment_id {
            let session = self.session()?;
            if session.is_locked() {
                None
            } else {
                let cipher = session.get_cipher(item_id).await?;
                let keys = session.keys()?;
                attachment_named(&keys, &cipher, old).ok().map(|(name, _)| name)
            }
        } else {
            None
        };
        let (approved, cipher, _draft) = self
            .item_gate(caller, VaultTool::AttachmentPut, item_id, |name, draft| {
                let replaced = replacement_label(replace_attachment_id, replaced_name);
                draft.attachment = Some(VaultAttachmentDraft {
                    name: file_name.clone(),
                    size,
                    replaces: replaced.clone(),
                });
                match &replaced {
                    Some(old) => format!(
                        "replace '{old}' on '{}' with '{file_name}'",
                        name.unwrap_or(item_id)
                    ),
                    None => {
                        format!("upload '{file_name}' to '{}'", name.unwrap_or(item_id))
                    }
                }
            })
            .await?;
        let result = async {
            let (wrapped, encrypted_name, blob) =
                approved.keys.prepare_attachment(&cipher, &file_name, &data)?;
            if blob.len() as u64 > MAX_ATTACHMENT_BYTES {
                return Err(VaultToolError::Message(format!(
                    "the encrypted attachment is {} bytes, over the {} byte cap",
                    blob.len(),
                    MAX_ATTACHMENT_BYTES
                )));
            }
            let slot_body = json!({
                "key": wrapped,
                "fileName": encrypted_name,
                "fileSize": blob.len() as u64,
                "adminRequest": false,
            });
            let slot = approved.session.create_attachment_slot(item_id, &slot_body).await?;
            let attachment_id =
                slot.get("attachmentId").and_then(Value::as_str).ok_or_else(|| {
                    VaultToolError::Message(
                        "the vault did not return the new attachment id".to_owned(),
                    )
                })?;
            approved.session.upload_attachment(item_id, attachment_id, blob.clone()).await?;
            let meta = approved.session.get_attachment_meta(item_id, attachment_id).await?;
            check_stored_size(meta.size, blob.len())?;
            // The replace's old attachment is deleted only after the new one
            // is stored and its size checked; a failed upload leaves the old
            // there.
            if let Some(old) = replace_attachment_id {
                approved.session.delete_attachment(item_id, old).await?;
            }
            Ok(json!({
                "id": attachment_id,
                "name": file_name,
                "size": blob.len(),
                "sha256": forge_connectors::vault::crypto::sha256_hex(&data),
                "replaced": replace_attachment_id,
            }))
        }
        .await;
        approved.done(result)
    }

    async fn folder_list(&self, caller: &SessionSlot) -> Result<Value, VaultToolError> {
        let draft = VaultDraft::new(VaultTool::FolderList, "list folders");
        let approved = self.gate(caller, draft.clone(), VaultTool::FolderList, None).await?;
        let result = async {
            let folders = approved.session.list_folders().await?;
            let rows: Vec<Value> = folders
                .iter()
                .map(|folder| {
                    json!({
                        "id": folder.id,
                        "name": approved.keys.decrypt_user_str(&folder.name).unwrap_or_else(|_| "(unreadable)".to_owned()),
                    })
                })
                .collect();
            Ok(json!({ "folders": rows }))
        }
        .await;
        approved.done(result)
    }

    async fn folder_create(
        &self,
        caller: &SessionSlot,
        name: &str,
    ) -> Result<Value, VaultToolError> {
        let mut draft = VaultDraft::new(VaultTool::FolderCreate, format!("create folder '{name}'"));
        draft.item_name = Some(name.to_owned());
        let approved = self.gate(caller, draft.clone(), VaultTool::FolderCreate, None).await?;
        let result = async {
            let encrypted = approved.keys.encrypt_user_str(name)?;
            let created = approved.session.create_folder(&encrypted).await?;
            let id = created.get("id").and_then(Value::as_str).ok_or_else(|| {
                VaultToolError::Message("the vault did not return the new folder's id".to_owned())
            })?;
            Ok(json!({ "id": id, "name": name }))
        }
        .await;
        approved.done(result)
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
        let approved = self.gate(caller, draft.clone(), VaultTool::FolderUpdate, None).await?;
        let result = async {
            let encrypted = approved.keys.encrypt_user_str(name)?;
            approved.session.update_folder(id, &encrypted).await?;
            Ok(json!({ "id": id, "name": name }))
        }
        .await;
        approved.done(result)
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
        let approved = self.gate(caller, draft.clone(), VaultTool::FolderDelete, None).await?;
        let result = async {
            approved.session.delete_folder(id).await?;
            let name = draft.item_name.clone().unwrap_or_else(|| id.to_owned());
            Ok(json!({ "id": id, "name": name, "deleted": true }))
        }
        .await;
        approved.done(result)
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

/// The server records the ciphertext's size at upload; a missing size fails
/// closed rather than reading as a match.
fn check_stored_size(stored: Option<u64>, blob_len: usize) -> Result<(), VaultToolError> {
    let Some(stored) = stored else {
        return Err(VaultToolError::Message(
            "the server did not report the stored attachment size".to_owned(),
        ));
    };
    if stored != blob_len as u64 {
        return Err(VaultToolError::Message(format!(
            "the server stored {stored} bytes for a {blob_len} byte blob"
        )));
    }
    Ok(())
}

/// A decryption failure outside an approved call still means the keys are
/// stale: drop them before handing the error back.
fn decrypt_failed(session: &VaultSession, error: VaultToolError) -> VaultToolError {
    if matches!(&error, VaultToolError::Vault(VaultError::Crypto(_)) | VaultToolError::Crypto(_)) {
        session.note_decrypt_failure();
    }
    error
}

/// The name a replace card draws for the attachment it would remove: the
/// decrypted file name when the session could read it, else the id.
fn replacement_label(id: Option<&str>, name: Option<String>) -> Option<String> {
    id.map(|id| name.unwrap_or_else(|| id.to_owned()))
}

/// Read an attachment source: the size is checked from metadata before the
/// read, and again after it, because the file can grow between the two.
fn read_attachment(path: &Path, cap: u64) -> Result<Vec<u8>, VaultToolError> {
    let declared = std::fs::metadata(path)?.len();
    if declared > cap {
        return Err(VaultToolError::Message(format!(
            "the file is {declared} bytes, over the {cap} byte cap"
        )));
    }
    let data = std::fs::read(path)?;
    if data.len() as u64 > cap {
        return Err(VaultToolError::Message(format!(
            "the file is {} bytes, over the {cap} byte cap",
            data.len()
        )));
    }
    Ok(data)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), VaultToolError> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // `O_NOFOLLOW` refuses a final-component symlink, and set_permissions on
    // the held fd fixes an existing looser mode, which `mode` only applies at
    // creation.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.flush()?;
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
    use crate::protocol::Command;
    use crate::vault::test_session;
    use forge_primitives::vault::{VaultAskAnswer, VaultAskDecision};

    fn audit_text(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(
            dir.path()
                .join("vault")
                .join(format!("audit-{}.jsonl", forge_sdk::config_dir_store_key(dir.path()))),
        )
        .unwrap_or_default()
    }

    async fn read_test_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read]);
            if let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Serve one response per expected `(path, body)` in order, asserting the
    /// request line names the path the test expected.
    async fn serve_vault(responses: Vec<(&'static str, String)>) -> std::net::SocketAddr {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for (path, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let request = read_test_request(&mut socket).await;
                assert!(
                    request.split_whitespace().nth(1) == Some(path),
                    "the server expected {path}, got {}",
                    request.lines().next().unwrap_or_default(),
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        address
    }

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
        let session = Arc::new(test_session(&config, None, Path::new("/tmp")).expect("a session"));
        *workspace.vault.write() = Some(session);
        let facade = Arc::new(ProdVaultFacade::from_arc(&workspace));
        (workspace, facade, SessionSlot::lead("Default", "forge"), rx)
    }

    async fn parked_draft(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
    ) -> VaultDraft {
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("the locked call parks a card")
                .expect("the seat stream stays open");
            if let crate::protocol::SessionUpdate::VaultAskPending { draft, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                return draft;
            }
        }
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

    fn locked_worker_workspace() -> (
        Arc<Workspace>,
        Arc<ProdVaultFacade>,
        SessionSlot,
        tempfile::TempDir,
        tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
    ) {
        use crate::mcp::workers::types::WorkerEntry;
        use forge_primitives::WorkerLiveness;
        use forge_primitives::project_key::ProjectKey;

        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, rx) = Workspace::testing_stub();
        let caller = SessionSlot::worker("TestOrg", "proj", "vault-test");
        workspace.insert_live_worker(
            &ProjectKey::new("TestOrg/proj"),
            WorkerEntry {
                label: "vault-test".to_owned(),
                charter: "test".to_owned(),
                slot: caller.clone(),
                session_id: None,
                status: WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead("TestOrg", "proj"),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: Some(dir.path().to_path_buf()),
                diagnostic: None,
                kick: None,
            },
        );
        let config = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "hi@vedhavyas.dev".to_owned(),
        };
        let session =
            Arc::new(test_session(&config, Some(dir.path()), dir.path()).expect("a session"));
        *workspace.vault.write() = Some(session);
        let facade = Arc::new(ProdVaultFacade::from_arc(&workspace));
        (workspace, facade, caller, dir, rx)
    }

    async fn held<T, F>(
        workspace: &Arc<Workspace>,
        caller: &SessionSlot,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
        tool: VaultTool,
        call: F,
    ) where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, VaultToolError>> + Send + 'static,
    {
        let task = tokio::spawn(call);
        let draft = parked_draft(rx).await;
        assert_eq!(draft.tool, tool, "the card names the tool that parked");
        decline(workspace, caller, draft.id);
        assert!(
            matches!(task.await.expect("the call finishes"), Err(VaultToolError::Rejected)),
            "{} must not run",
            tool.as_str(),
        );
    }

    /// Every tool parks its own card on a locked session and runs nothing
    /// until the reader answers.
    #[tokio::test]
    async fn every_tool_parks_its_own_card_on_a_locked_session() {
        let (workspace, facade, caller, dir, mut rx) = locked_worker_workspace();
        std::fs::write(dir.path().join("note.txt"), b"hello").expect("the file");

        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, VaultTool::Search, async move {
                f.search(&c, "bank", None).await
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, VaultTool::Get, async move {
                f.get(&c, "item-1", Some("password")).await
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            let input = CreateInput { name: "bank".to_owned(), ..Default::default() };
            held(&workspace, &caller, &mut rx, VaultTool::Create, async move {
                f.create(&c, &input).await
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            let patch = PatchInput { password: Some("x".to_owned()), ..Default::default() };
            held(&workspace, &caller, &mut rx, VaultTool::Update, async move {
                f.update(&c, "item-1", &patch).await
            })
            .await;
        }
        for (tool, id) in [
            (VaultTool::Trash, "item-1"),
            (VaultTool::Restore, "item-1"),
            (VaultTool::TotpCode, "item-1"),
        ] {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, tool, async move {
                match tool {
                    VaultTool::Trash => f.trash(&c, id).await,
                    VaultTool::Restore => f.restore(&c, id).await,
                    _ => f.totp_code(&c, id).await,
                }
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, VaultTool::AttachmentGet, async move {
                f.attachment_get(&c, "item-1", "att-1", "out.bin").await
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, VaultTool::AttachmentPut, async move {
                f.attachment_put(&c, "item-1", "note.txt", None, None).await
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, VaultTool::FolderList, async move {
                f.folder_list(&c).await
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, VaultTool::FolderCreate, async move {
                f.folder_create(&c, "folder").await
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, VaultTool::FolderUpdate, async move {
                f.folder_update(&c, "folder-id", "renamed").await
            })
            .await;
        }
        {
            let f = Arc::clone(&facade);
            let c = caller.clone();
            held(&workspace, &caller, &mut rx, VaultTool::FolderDelete, async move {
                f.folder_delete(&c, "folder-id").await
            })
            .await;
        }

        let audit = std::fs::read_to_string(
            dir.path()
                .join("vault")
                .join(format!("audit-{}.jsonl", forge_sdk::config_dir_store_key(dir.path()))),
        )
        .expect("every decline wrote an audit line");
        assert_eq!(audit.lines().count(), 13, "one line per decision");
        assert!(
            audit.contains("\"field\":\"password\""),
            "the get's reveal field reaches the audit through the gate",
        );
    }

    /// The line an approval leaves records the operation's own outcome, not
    /// a claim made before it ran, and the field the gate carried reaches it.
    #[test]
    fn an_approved_call_audits_its_own_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "hi@vedhavyas.dev".to_owned(),
        };
        let session =
            Arc::new(test_session(&config, Some(dir.path()), dir.path()).expect("a session"));
        let approved = Approved {
            workspace: Arc::clone(&Workspace::testing_stub().0),
            session,
            keys: Arc::new(Unlocked {
                user_key: forge_connectors::vault::crypto::make_symmetric_key(),
                private_key: None,
                org_keys: std::collections::HashMap::new(),
                failed_orgs: Vec::new(),
            }),
            caller: SessionSlot::lead("Default", "forge"),
            tool: VaultTool::Get,
            item_id: Some("cipher-id".to_owned()),
            item_name: Some("bank".to_owned()),
            field: Some("password".to_owned()),
        };

        let failed: Result<(), VaultToolError> = Err(VaultToolError::Message("boom".to_owned()));
        let _ = approved.done(failed);
        let _ = approved.done(Ok(()));

        let path = dir
            .path()
            .join("vault")
            .join(format!("audit-{}.jsonl", forge_sdk::config_dir_store_key(dir.path())));
        let audit = std::fs::read_to_string(path).expect("the outcome lines");
        let lines: Vec<serde_json::Value> =
            audit.lines().map(|line| serde_json::from_str(line).expect("json")).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["tool"], "vault__get");
        assert_eq!(lines[0]["item_id"], "cipher-id");
        assert_eq!(lines[0]["item_name"], "bank");
        assert_eq!(lines[0]["field"], "password");
        assert_eq!(lines[0]["decision"], "approve");
        assert_eq!(lines[0]["result"], "failed: boom");
        assert_eq!(lines[1]["result"], "ok", "a success is recorded after the call runs");
    }

    /// A locked session must park the card asking for the password before it
    /// makes any token call. The first call after a forge start is the case:
    /// an error returned before the hold would leave the reader no way to
    /// unlock.
    #[tokio::test]
    async fn a_locked_get_parks_the_unlock_card_before_any_token_call() {
        let (workspace, facade, caller, mut rx) = locked_workspace();
        let asked = caller.clone();
        let task = tokio::spawn(async move { facade.get(&asked, "item-1", None).await });

        let draft = parked_draft(&mut rx).await;
        assert!(draft.needs_unlock, "the card asks for the master password");
        assert_eq!(draft.item_id.as_deref(), Some("item-1"));
        assert_eq!(draft.summary, "read metadata for item item-1");

        decline(&workspace, &caller, draft.id);
        assert!(
            matches!(task.await.expect("the call finishes"), Err(VaultToolError::Rejected)),
            "a decline runs nothing"
        );
    }

    /// A send-back is not an approval: the note reaches the agent as the
    /// call's own error and no operation runs in the vault.
    #[tokio::test]
    async fn a_send_back_returns_the_note_and_runs_nothing() {
        let (workspace, facade, caller, mut rx) = locked_workspace();
        let asked = caller.clone();
        let task = tokio::spawn(async move { facade.search(&asked, "bank", None).await });

        let draft = parked_draft(&mut rx).await;
        workspace
            .dispatch(Command::RespondVaultAsk {
                key: caller.clone(),
                id: draft.id,
                answer: VaultAskAnswer {
                    decision: VaultAskDecision::SendBack { note: "use the bank folder".to_owned() },
                    master_password: None,
                    two_factor: None,
                },
            })
            .expect("the answer routes");

        match task.await.expect("the call finishes") {
            Err(VaultToolError::SentBack { note }) => assert_eq!(note, "use the bank folder"),
            other => panic!("a send-back must not run the search: {other:?}"),
        }
    }

    /// The attachment writer creates a private file and repairs an existing
    /// file's mode, which `mode(0o600)` alone does not do.
    #[test]
    fn write_private_creates_and_repairs_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note");
        write_private(&path, b"first").expect("write");
        assert_eq!(
            std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600,
            "a new attachment is private",
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private(&path, b"second").expect("write");
        assert_eq!(
            std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600,
            "an existing attachment is repaired to private",
        );
    }

    /// A final-component symlink is refused rather than followed out of the
    /// session's tree.
    #[test]
    fn write_private_refuses_a_final_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        std::fs::write(&target, b"x").expect("target");
        let link = dir.path().join("link");
        symlink(&target, &link).expect("symlink");

        assert!(write_private(&link, b"y").is_err(), "the symlink is not followed");
        assert_eq!(std::fs::read(&target).expect("target read"), b"x", "the target is untouched");
    }

    /// The upload size check is a comparison only when the server reported a
    /// size; a missing size is an error, not a match.
    #[test]
    fn a_missing_upload_size_fails_closed() {
        assert!(check_stored_size(Some(97), 97).is_ok());
        assert!(check_stored_size(Some(96), 97).is_err());
        assert!(check_stored_size(None, 97).is_err(), "a missing size is not a match");
    }

    /// A name patch leaves the fields this version does not model intact and
    /// encrypts the new name with the cipher's own key.
    #[test]
    fn a_name_patch_keeps_the_unmodelled_fields() {
        let unlocked = Unlocked {
            user_key: forge_connectors::vault::crypto::make_symmetric_key(),
            private_key: None,
            org_keys: std::collections::HashMap::new(),
            failed_orgs: Vec::new(),
        };
        let cipher = serde_json::json!({
            "id": "c",
            "type": 1,
            "name": "old",
            "login": { "totp": "seed", "password": "pw" },
            "fields": [{ "name": "n", "value": "v" }],
            "passwordHistory": [{ "password": "old" }],
            "key": null,
        });
        let patch = PatchInput { name: Some("renamed".to_owned()), ..Default::default() };

        let out = apply_patch(&unlocked, cipher.clone(), &patch).expect("the patch");

        assert_eq!(out["login"]["totp"], cipher["login"]["totp"]);
        assert_eq!(out["login"]["password"], cipher["login"]["password"]);
        assert_eq!(out["fields"], cipher["fields"]);
        assert_eq!(out["passwordHistory"], cipher["passwordHistory"]);
        assert_eq!(out["key"], cipher["key"]);
        let name = out["name"].as_str().expect("the name is an EncString");
        assert_eq!(unlocked.decrypt_item_str(&out, name).expect("decrypt"), "renamed");
    }

    /// A new login encrypts its fields with the user key and keeps the type
    /// the caller asked for.
    #[test]
    fn a_new_login_encrypts_its_fields() {
        let unlocked = Unlocked {
            user_key: forge_connectors::vault::crypto::make_symmetric_key(),
            private_key: None,
            org_keys: std::collections::HashMap::new(),
            failed_orgs: Vec::new(),
        };
        let input = CreateInput {
            name: "bank".to_owned(),
            username: Some("u".to_owned()),
            password: Some("p".to_owned()),
            ..Default::default()
        };

        let body = build_new_cipher(&unlocked, &input, "login").expect("the body");

        assert_eq!(body["type"], 1);
        let name = body["name"].as_str().expect("the name is an EncString");
        assert_eq!(unlocked.decrypt_item_str(&body, name).expect("decrypt"), "bank");
        let username = body.pointer("/login/username").and_then(Value::as_str).expect("username");
        assert_eq!(unlocked.decrypt_item_str(&body, username).expect("decrypt"), "u");
    }

    /// Removing the containment checks would let a prompt-injected call read
    /// `~/.ssh/id_rsa` or write outside the session's tree.
    #[test]
    fn the_attachment_paths_must_stay_inside_the_session_tree() {
        let (workspace, _facade, caller, dir, _rx) = locked_worker_workspace();
        std::fs::write(dir.path().join("ok.txt"), b"x").expect("the file");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), b"x").expect("the outside file");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");

        assert!(resolve_source(&workspace, &caller, "ok.txt").is_ok());
        assert!(resolve_destination(&workspace, &caller, "out.txt").is_ok());

        for escape in ["../escape.txt", "/etc/hosts", "link/secret"] {
            assert!(
                resolve_source(&workspace, &caller, escape).is_err(),
                "the source {escape} escapes the tree",
            );
        }
        for escape in ["../escape.txt", "/tmp/escape.txt", "link/out.txt"] {
            assert!(
                resolve_destination(&workspace, &caller, escape).is_err(),
                "the destination {escape} escapes the tree",
            );
        }
        assert!(!outside.path().join("out.txt").exists(), "nothing was written outside");
    }

    /// A replace card names the file it removes when the session could read
    /// it, and falls back to the id only when it could not.
    #[test]
    fn a_replace_card_names_the_file_it_removes() {
        assert_eq!(
            replacement_label(Some("att-1"), Some("old.txt".to_owned())).as_deref(),
            Some("old.txt"),
        );
        assert_eq!(replacement_label(Some("att-1"), None).as_deref(), Some("att-1"));
        assert_eq!(replacement_label(None, Some("old.txt".to_owned())), None);
    }

    /// The source size is checked from metadata before the read, and a file
    /// that grew past the cap between the check and the read is refused too.
    #[test]
    fn an_attachment_source_over_the_cap_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file");
        std::fs::write(&path, b"five!").expect("the file");

        let error = read_attachment(&path, 4).expect_err("the cap refuses the file");
        assert!(error.to_string().contains("over the 4 byte cap"), "{error}");
        assert_eq!(read_attachment(&path, 5).expect("the exact size passes"), b"five!");
    }

    /// A locked attachment-get card cannot name the item, but it must name
    /// where the file would land.
    #[tokio::test]
    async fn a_locked_attachment_get_card_names_its_destination() {
        let (workspace, facade, caller, _dir, mut rx) = locked_worker_workspace();
        let asked = caller.clone();
        let task = tokio::spawn(async move {
            facade.attachment_get(&asked, "item-1", "att-1", "out.bin").await
        });

        let draft = parked_draft(&mut rx).await;
        assert!(draft.needs_unlock);
        assert!(draft.item_name.is_none(), "a locked card cannot name the item");
        assert!(
            draft.summary.contains("out.bin"),
            "the card names the destination: {}",
            draft.summary,
        );

        decline(&workspace, &caller, draft.id);
        assert!(matches!(task.await.expect("the call finishes"), Err(VaultToolError::Rejected)));
    }

    /// A `[vault]` reload builds a new session; an approval drawn against the
    /// old account must not run against it.
    #[tokio::test]
    async fn an_approval_does_not_run_after_a_config_reload() {
        let (workspace, facade, caller, dir, mut rx) = locked_worker_workspace();
        let asked = caller.clone();
        let task = tokio::spawn(async move { facade.search(&asked, "bank", None).await });
        let draft = parked_draft(&mut rx).await;

        let next = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "other@example.com".to_owned(),
        };
        *workspace.vault.write() = Some(Arc::new(
            test_session(&next, Some(dir.path()), dir.path()).expect("the new session"),
        ));
        workspace
            .dispatch(Command::RespondVaultAsk {
                key: caller.clone(),
                id: draft.id,
                answer: VaultAskAnswer {
                    decision: VaultAskDecision::Approve,
                    master_password: Some("old password".to_owned()),
                    two_factor: None,
                },
            })
            .expect("the answer routes");

        let result = task.await.expect("the call finishes");
        assert!(
            matches!(&result, Err(VaultToolError::Message(message)) if message.contains("configuration changed")),
            "{result:?}",
        );
        let audit = audit_text(&dir);
        assert!(
            audit.contains("failed: the vault configuration changed"),
            "the mismatch is audited: {audit}",
        );
    }

    /// An approval that carries no password cannot unlock, and the refusal is
    /// audited rather than settling the card silently.
    #[tokio::test]
    async fn a_locked_approval_without_a_password_is_audited() {
        let (workspace, facade, caller, dir, mut rx) = locked_worker_workspace();
        let asked = caller.clone();
        let task = tokio::spawn(async move { facade.search(&asked, "bank", None).await });
        let draft = parked_draft(&mut rx).await;

        workspace
            .dispatch(Command::RespondVaultAsk {
                key: caller.clone(),
                id: draft.id,
                answer: VaultAskAnswer {
                    decision: VaultAskDecision::Approve,
                    master_password: None,
                    two_factor: None,
                },
            })
            .expect("the answer routes");

        assert!(matches!(task.await.expect("the call finishes"), Err(VaultToolError::Locked)));
        let audit = audit_text(&dir);
        assert!(
            audit.contains("failed: a locked session was approved without the master password"),
            "{audit}",
        );
    }

    /// Approve -> unlock -> rebase, end to end: the first card carries the
    /// password, the second is restamped by the unlock and runs without one.
    #[tokio::test]
    async fn an_unlock_rebases_the_other_parked_card_end_to_end() {
        use forge_connectors::vault::crypto::{derive_master_key, encrypt_str};

        let pre = forge_connectors::vault::models::Prelogin {
            kdf: 0,
            kdf_iterations: 100_000,
            kdf_memory: None,
            kdf_parallelism: None,
        };
        let master_key =
            derive_master_key("open sesame", "hi@vedhavyas.dev", &pre).expect("the master key");
        let (user_key, wrapped_user_key) = master_key.make_user_key().expect("the user key");
        let cipher = serde_json::json!({
            "id": "item-1",
            "type": 1,
            "name": encrypt_str("bank", &user_key.0).expect("encrypting the name"),
            "login": {},
        });
        let prelogin = serde_json::json!({
            "kdf": 0,
            "kdfIterations": 100_000,
        })
        .to_string();
        let token = serde_json::json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "Key": wrapped_user_key.to_string(),
        })
        .to_string();
        let sync = r#"{"profile":{},"folders":[],"ciphers":[]}"#.to_owned();
        let address = serve_vault(vec![
            ("/identity/accounts/prelogin", prelogin),
            ("/identity/connect/token", token),
            ("/api/sync", sync),
            ("/api/ciphers/item-1", cipher.to_string()),
            ("/api/ciphers/item-1", cipher.to_string()),
        ])
        .await;
        let (workspace, mut rx) = Workspace::testing_stub();
        let config =
            VaultConfig { url: format!("http://{address}"), email: "hi@vedhavyas.dev".to_owned() };
        let session = Arc::new(test_session(&config, None, Path::new("/tmp")).expect("session"));
        *workspace.vault.write() = Some(session);
        let facade = Arc::new(ProdVaultFacade::from_arc(&workspace));
        let caller = SessionSlot::lead("Default", "forge");

        let first_facade = Arc::clone(&facade);
        let first_caller = caller.clone();
        let first =
            tokio::spawn(async move { first_facade.get(&first_caller, "item-1", None).await });
        let first_draft = parked_draft(&mut rx).await;
        assert!(first_draft.needs_unlock, "the first card asks for the password");

        let second_facade = Arc::clone(&facade);
        let second_caller = caller.clone();
        let second =
            tokio::spawn(async move { second_facade.get(&second_caller, "item-1", None).await });
        let second_draft = parked_draft(&mut rx).await;
        assert!(second_draft.needs_unlock, "the second card parks while locked too");

        workspace
            .dispatch(Command::RespondVaultAsk {
                key: caller.clone(),
                id: first_draft.id,
                answer: VaultAskAnswer {
                    decision: VaultAskDecision::Approve,
                    master_password: Some("open sesame".to_owned()),
                    two_factor: None,
                },
            })
            .expect("the answer routes");

        let mut restamped = None;
        while restamped.is_none() {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("the unlock pushes a restamp")
                .expect("the seat stream stays open");
            if let crate::protocol::SessionUpdate::VaultAskPending { draft, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
                && draft.id == second_draft.id
            {
                restamped = Some(draft);
            }
        }
        let restamped = restamped.expect("the second card was restamped");
        assert!(!restamped.needs_unlock, "the unlock restamps the other card");

        let decoded = first.await.expect("the first call finishes").expect("the first read");
        assert_eq!(decoded["name"], "bank");

        workspace
            .dispatch(Command::RespondVaultAsk {
                key: caller.clone(),
                id: second_draft.id,
                answer: VaultAskAnswer {
                    decision: VaultAskDecision::Approve,
                    master_password: None,
                    two_factor: None,
                },
            })
            .expect("the answer routes");
        let decoded = second.await.expect("the second call finishes").expect("the second read");
        assert_eq!(decoded["name"], "bank");
    }

    /// A locked search parks the query rather than failing on the sync.
    #[tokio::test]
    async fn a_locked_search_parks_the_query() {
        let (workspace, facade, caller, mut rx) = locked_workspace();
        let asked = caller.clone();
        let task = tokio::spawn(async move { facade.search(&asked, "bank", None).await });

        let draft = parked_draft(&mut rx).await;
        assert!(draft.needs_unlock);
        assert!(draft.summary.contains("search for 'bank'"), "{}", draft.summary);

        decline(&workspace, &caller, draft.id);
        assert!(matches!(task.await.expect("the call finishes"), Err(VaultToolError::Rejected)));
    }
}
