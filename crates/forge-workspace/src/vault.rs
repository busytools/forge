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
use forge_primitives::vault::{VaultReleased, VaultTool};
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

use crate::SessionSlot;
use crate::config::VaultConfig;
use crate::protocol::SessionUpdate;
use crate::update_fanout::UpdateFanout;

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
    /// Serializes refreshes. A rotating refresh token is spent once, so two
    /// concurrent 401s must not both redeem it; the second reads the token
    /// the first stored.
    refresh_gate: tokio::sync::Mutex<()>,
    /// Where a rotation is announced: the enrolment blobs hold the old token
    /// and the client that owns one rewrites it from this.
    updates: UpdateFanout,
    audit: Option<VaultAudit>,
}

impl VaultSession {
    /// Build the session's audit around a client the workspace built. The
    /// audit path is `<app-support>/vault/audit-<config-dir-store-key>.jsonl`;
    /// a missing app-support dir leaves the session usable and the audit off.
    pub(crate) fn with_client(
        config: &VaultConfig,
        app_support: Option<&Path>,
        config_dir: &Path,
        client: VaultClient,
        updates: UpdateFanout,
    ) -> Result<Self, VaultError> {
        let audit = app_support.and_then(|base| {
            match VaultAudit::new(base, &forge_sdk::config_dir_store_key(config_dir)) {
                Ok(audit) => Some(audit),
                Err(error) => {
                    tracing::warn!(
                        event_name = "vault_audit_open_failed",
                        path = %base.join("vault").display(),
                        %error,
                        "the vault audit could not be opened; operations will not be recorded"
                    );
                    None
                }
            }
        });
        Ok(Self {
            client,
            email: config.email.clone(),
            state: parking_lot::RwLock::new(SessionState::default()),
            unlock_gate: tokio::sync::Mutex::new(()),
            refresh_gate: tokio::sync::Mutex::new(()),
            updates,
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

    /// The material a client wraps to enrol this device: the current refresh
    /// token and the account's user key, base64 - or `None` when the session
    /// holds no tokens or keys, and when it holds no refresh token, because a
    /// blob built on material that cannot refresh is a shortcut to nothing.
    pub(crate) fn enrolment_material(&self) -> Option<VaultReleased> {
        let state = self.state.read();
        let keys = state.keys.as_ref()?;
        let tokens = state.tokens.as_ref()?;
        let refresh = tokens.refresh.as_deref().filter(|token| !token.is_empty())?;
        Some(VaultReleased {
            refresh_token: refresh.to_owned(),
            user_key: keys.user_key.to_base64().to_string(),
        })
    }

    /// Tell every view the refresh token rotated. The token is a secret: it
    /// rides the live socket for the client that owns an enrolment to rewrite
    /// its blob, and no store keeps it.
    fn announce_rotation(&self, refresh_token: String) {
        let _ = self.updates.send(SessionUpdate::VaultRotated { refresh_token });
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
        // `bitwarden-crypto`'s `MasterKey` has no public zeroize; it is
        // dropped as soon as the user and private keys are unwrapped.
        let master_key = crypto::derive_master_key(password, &self.email, &prelogin)
            .map_err(VaultError::Crypto)?;
        let hash = Zeroizing::new(crypto::master_password_hash(&master_key, password));
        let mut two_factor =
            two_factor.map(|token| TwoFactorCode { token: token.to_owned(), provider: 0 });
        let login = self.client.login_password(&self.email, &hash, two_factor.as_ref()).await;
        if let Some(code) = &mut two_factor {
            code.token.zeroize();
        }
        // Every failure below drops the token through its `Drop`, which wipes
        // the access and refresh tokens and the wrapped keys.
        let mut token: TokenResponse = login?;
        let sync = self.client.sync(&token.access_token).await?;
        let unlocked =
            crypto::unlock(&token, &master_key, &sync.profile).map_err(VaultError::Crypto)?;
        if !unlocked.failed_orgs.is_empty() {
            tracing::warn!(
                event_name = "vault_org_keys_unavailable",
                orgs = ?unlocked.failed_orgs,
                "organization keys were not unlocked; their items are refused until the next unlock"
            );
        }

        let mut state = self.state.write();
        state.tokens = Some(Tokens {
            access: std::mem::take(&mut token.access_token),
            refresh: token.refresh_token.take(),
        });
        state.keys = Some(std::sync::Arc::new(unlocked));
        Ok(())
    }

    /// Unlock from material a client released through its platform
    /// authenticator: the enrolment's refresh token and the account's user
    /// key.
    ///
    /// The destination is the password unlock's - a token, a sync and the
    /// held keys - without the master password crossing on this start. The
    /// server cannot prove the released user key (client-side enforcement is
    /// the design); a wrong one fails when the private key will not unwrap.
    /// Idempotent like [`Self::unlock`]: a session another card unlocked is
    /// left alone.
    pub(crate) async fn unlock_released(
        &self,
        refresh_token: &str,
        user_key: &str,
    ) -> Result<(), VaultError> {
        let _gate = self.unlock_gate.lock().await;
        if !self.is_locked() {
            return Ok(());
        }

        // The token is spent once and the endpoint rotates it; the response's
        // next refresh token is what this session keeps for its own refreshes.
        let mut token: TokenResponse = self.client.refresh(refresh_token).await?;
        let sync = self.client.sync(&token.access_token).await?;
        let unlocked = crypto::unlock_with_user_key(&token, user_key, &sync.profile)
            .map_err(VaultError::Crypto)?;
        if !unlocked.failed_orgs.is_empty() {
            tracing::warn!(
                event_name = "vault_org_keys_unavailable",
                orgs = ?unlocked.failed_orgs,
                "organization keys were not unlocked; their items are refused until the next unlock"
            );
        }

        let mut state = self.state.write();
        // The redemption spent the token the client released, so the lineage
        // rotated: the client rewrites its blob with the one this session
        // now holds.
        let rotated = token.refresh_token.clone();
        state.tokens = Some(Tokens {
            access: std::mem::take(&mut token.access_token),
            refresh: token.refresh_token.take(),
        });
        state.keys = Some(std::sync::Arc::new(unlocked));
        drop(state);
        if let Some(refresh_token) = rotated {
            self.announce_rotation(refresh_token);
        }
        Ok(())
    }

    /// The current access token, or `Locked` when there is none. The clone is
    /// zeroized when the caller drops it.
    fn access_token(&self) -> Result<Zeroizing<String>, VaultError> {
        self.state
            .read()
            .tokens
            .as_ref()
            .map(|tokens| Zeroizing::new(tokens.access.clone()))
            .ok_or(VaultError::Locked)
    }

    /// Refresh the access token. A missing or rejected refresh token clears
    /// the keys and tokens, which is what makes the next held call ask for
    /// the master password; a session with no refresh token must lock too,
    /// or the card would never ask and every call would repeat the 401.
    ///
    /// `spent` is the token that got the 401. Another call may have refreshed
    /// while this one waited on the gate; its token is then current and the
    /// refresh token was already spent, so that answer is handed back instead
    /// of redeeming the same refresh token twice.
    pub(crate) async fn refresh(&self, spent: &str) -> Result<Zeroizing<String>, VaultError> {
        let _gate = self.refresh_gate.lock().await;
        let current =
            self.state.read().tokens.as_ref().map(|tokens| Zeroizing::new(tokens.access.clone()));
        match current {
            None => return Err(VaultError::Locked),
            Some(access) if access.as_str() != spent => return Ok(access),
            Some(access) => drop(access),
        }
        let Some(mut refresh) = ({
            let state = self.state.read();
            state.tokens.as_ref().and_then(|tokens| tokens.refresh.clone())
        }) else {
            self.lock();
            return Err(VaultError::Locked);
        };
        match self.client.refresh(&refresh).await {
            Ok(mut token) => {
                let access = Zeroizing::new(token.access_token.clone());
                let next_refresh =
                    token.refresh_token.take().or_else(|| Some(std::mem::take(&mut refresh)));
                refresh.zeroize();
                let rotated = next_refresh.clone();
                if let Some(tokens) = self.state.write().tokens.as_mut() {
                    tokens.access.zeroize();
                    if let Some(mut old) = tokens.refresh.take() {
                        old.zeroize();
                    }
                    tokens.access = std::mem::take(&mut token.access_token);
                    tokens.refresh = next_refresh;
                }
                if let Some(refresh_token) = rotated {
                    self.announce_rotation(refresh_token);
                }
                Ok(access)
            }
            Err(error) => {
                refresh.zeroize();
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

    /// A decryption failure under a key that has not changed means the account
    /// rotated its keys: drop them, so the next held call asks for the master
    /// password rather than repeating a failure against stale keys.
    pub(crate) fn note_decrypt_failure(&self, caller: &SessionSlot, item: Option<&str>) {
        tracing::warn!(
            event_name = "vault_decrypt_failed",
            caller = %caller.display(),
            item = item,
            vault = %self.client.base(),
            "a vault decryption failed; the session's keys are dropped and the next call will unlock"
        );
        self.lock();
    }

    /// A write must not run against keys the account rotated while the session
    /// stayed open: its ciphertext would be unreadable after the next unlock.
    /// The wrapped user key's fingerprint from unlock is compared with the
    /// one a fresh sync carries. The sync comes back so a write can stamp the
    /// account id from the same read that verified the key.
    pub(crate) async fn ensure_keys_current(
        &self,
        caller: &SessionSlot,
        item: Option<&str>,
    ) -> Result<SyncData, VaultError> {
        let keys = self.keys()?;
        let sync = self.sync().await?;
        let Some(profile_key) = sync.profile.key.as_deref().filter(|key| !key.is_empty()) else {
            // A sync that omits the key cannot prove the unlock still holds;
            // proceeding would be the corruption this check exists to stop.
            self.note_key_unverifiable(caller, item);
            return Err(VaultError::KeyUnverifiable);
        };
        if crypto::sha256_hex(profile_key.as_bytes()) != keys.user_key_fingerprint {
            self.note_key_rotation(caller, item);
            return Err(VaultError::KeyRotated);
        }
        Ok(sync)
    }

    /// Drop the keys when a sync cannot prove which user key the account
    /// holds, so the next call asks for the master password instead of
    /// writing under an unverified unlock.
    pub(crate) fn note_key_unverifiable(&self, caller: &SessionSlot, item: Option<&str>) {
        tracing::warn!(
            event_name = "vault_key_unverifiable",
            caller = %caller.display(),
            item = item,
            vault = %self.client.base(),
            "the sync carried no wrapped user key; the session is locked"
        );
        self.lock();
    }

    /// Drop the keys after an account rotation; re-entering the master
    /// password is the only thing that fixes it.
    pub(crate) fn note_key_rotation(&self, caller: &SessionSlot, item: Option<&str>) {
        tracing::warn!(
            event_name = "vault_keys_rotated",
            caller = %caller.display(),
            item = item,
            vault = %self.client.base(),
            "the account's wrapped user key changed while unlocked; the session is locked"
        );
        self.lock();
    }

    /// `GET /api/sync`, through the refresh path.
    pub(crate) async fn sync(&self) -> Result<SyncData, VaultError> {
        let token = self.access_token()?;
        match self.client.sync(&token).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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
        data: &[u8],
    ) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.upload_attachment(&token, cipher_id, attachment_id, data).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh(&token).await?;
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
                let token = self.refresh(&token).await?;
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

    pub(crate) async fn download_attachment(
        &self,
        url: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, VaultError> {
        let token = self.access_token()?;
        match self.client.download_attachment(&token, url, max_bytes).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh(&token).await?;
                match self.client.download_attachment(&token, url, max_bytes).await {
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
                let token = self.refresh(&token).await?;
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

impl crate::workspace::Workspace {
    /// The material a client wraps to enrol this device, from the shared
    /// session - or `None` when it is locked or no `[vault]` is configured.
    pub(crate) fn vault_enrolment_material(&self) -> Option<VaultReleased> {
        self.vault.read().as_ref()?.enrolment_material()
    }
}

/// Install an unlocked state on a test session without the login round-trip.
#[cfg(test)]
pub(crate) fn test_install_unlocked(session: &VaultSession, unlocked: Unlocked) {
    let mut state = session.state.write();
    state.tokens =
        Some(Tokens { access: "test-access".to_owned(), refresh: Some("test-refresh".to_owned()) });
    state.keys = Some(std::sync::Arc::new(unlocked));
}

/// A session over the connector's own client, for local test servers. The
/// production path builds the clients through [`production_client`].
#[cfg(test)]
pub(crate) fn test_session(
    config: &VaultConfig,
    app_support: Option<&Path>,
    config_dir: &Path,
) -> Result<VaultSession, VaultError> {
    test_session_with_updates(config, app_support, config_dir, UpdateFanout::default())
}

/// [`test_session`] over a fan-out the test holds, so it can read what the
/// session announces.
#[cfg(test)]
pub(crate) fn test_session_with_updates(
    config: &VaultConfig,
    app_support: Option<&Path>,
    config_dir: &Path,
    updates: UpdateFanout,
) -> Result<VaultSession, VaultError> {
    let client = VaultClient::new(&config.url)?;
    VaultSession::with_client(config, app_support, config_dir, client, updates)
}

/// The production vault clients: the machine's extra TLS roots, `https_only`,
/// the account's own user agent, and the vault family's bounds. The download
/// client takes a longer read and total bound because an attachment is opaque
/// ciphertext up to the server's cap.
pub(crate) fn production_client(base: &str) -> Result<VaultClient, VaultError> {
    use forge_connectors::vault::client::{
        CONNECT_TIMEOUT, DOWNLOAD_READ_TIMEOUT, DOWNLOAD_TIMEOUT, READ_TIMEOUT, REQUEST_TIMEOUT,
    };
    let build = |read_timeout: std::time::Duration, timeout: std::time::Duration| {
        forge_agent::http_trust::with_extra_roots(
            reqwest::Client::builder()
                .user_agent(forge_connectors::vault::client::user_agent())
                .https_only(true)
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(read_timeout)
                .timeout(timeout),
        )
        .build()
        .map_err(VaultError::Http)
    };
    VaultClient::with_clients(
        base,
        build(READ_TIMEOUT, REQUEST_TIMEOUT)?,
        build(DOWNLOAD_READ_TIMEOUT, DOWNLOAD_TIMEOUT)?,
    )
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
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let dir = app_support.join("vault");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("audit-{store_key}.jsonl"));
        // `O_NOFOLLOW` refuses a planted symlink, and the explicit
        // set_permissions fixes an existing looser file's mode, which `mode`
        // only applies at creation.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
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
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    match std::fs::remove_file(rotated_path(path, AUDIT_MAX_FILES)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for index in (1..AUDIT_MAX_FILES).rev() {
        let from = rotated_path(path, index);
        if from.exists() {
            std::fs::rename(&from, rotated_path(path, index + 1))?;
        }
    }
    // A failed rename is returned rather than followed by a truncate of the
    // file that was meant to move, which would destroy the audit.
    std::fs::rename(path, rotated_path(path, 1))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    audit.file = file;
    audit.size = 0;
    Ok(())
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{index}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> VaultConfig {
        VaultConfig { url: "http://127.0.0.1:1".to_owned(), email: "hi@vedhavyas.dev".to_owned() }
    }

    #[test]
    fn a_new_session_is_locked() {
        let session = test_session(&config(), None, Path::new("/tmp")).expect("a session");
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

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
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
        buf
    }

    /// Serve `responses` in order, one request per response.
    async fn serve(responses: Vec<String>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let _ = read_request(&mut socket).await;
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (address, handle)
    }

    fn response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn session_for(address: std::net::SocketAddr) -> VaultSession {
        let config =
            VaultConfig { url: format!("http://{address}"), email: "hi@vedhavyas.dev".to_owned() };
        let session = test_session(&config, None, Path::new("/tmp")).expect("a session");
        session.state.write().tokens =
            Some(Tokens { access: "stale".to_owned(), refresh: Some("good".to_owned()) });
        session
    }

    /// The material a device enrols with is the session's current refresh
    /// token and the account's user key, and nothing when either is missing.
    #[test]
    fn the_enrolment_material_is_the_refresh_token_and_user_key() {
        let session = test_session(&config(), None, Path::new("/tmp")).expect("a session");
        assert!(session.enrolment_material().is_none(), "a locked session enrols nothing");

        let key = crypto::make_symmetric_key();
        test_install_unlocked(
            &session,
            Unlocked {
                user_key: key.clone(),
                private_key: None,
                org_keys: std::collections::HashMap::new(),
                failed_orgs: Vec::new(),
                user_key_fingerprint: String::new(),
            },
        );
        let material = session.enrolment_material().expect("the unlocked material");
        assert_eq!(material.refresh_token, "test-refresh");
        assert_eq!(
            material.user_key,
            key.to_base64().to_string(),
            "the user key is the one the unlock holds",
        );

        if let Some(tokens) = session.state.write().tokens.as_mut() {
            tokens.refresh = None;
        }
        assert!(
            session.enrolment_material().is_none(),
            "a session with no refresh token enrols nothing: the blob could never refresh",
        );
    }

    /// A rotation is announced with the new token, and the frame's Debug never
    /// prints it.
    #[tokio::test]
    async fn a_rotation_announces_the_new_token_and_redacts_it() {
        use crate::update_fanout::SubscriberRole;

        let (address, server) = serve(vec![
            response("401 Unauthorized", ""),
            response("200 OK", r#"{"access_token":"fresh","refresh_token":"next"}"#),
            response("200 OK", r#"{"profile":{},"folders":[],"ciphers":[]}"#),
        ])
        .await;
        let updates = UpdateFanout::default();
        let mut announced = updates.subscribe(SubscriberRole::Answering);
        let config =
            VaultConfig { url: format!("http://{address}"), email: "hi@vedhavyas.dev".to_owned() };
        let session = test_session_with_updates(&config, None, Path::new("/tmp"), updates.clone())
            .expect("a session");
        session.state.write().tokens =
            Some(Tokens { access: "stale".to_owned(), refresh: Some("good".to_owned()) });

        session.sync().await.expect("the refresh recovers the call");

        let frame = announced.try_recv().expect("the rotation is announced");
        match &*frame.update {
            SessionUpdate::VaultRotated { refresh_token } => assert_eq!(refresh_token, "next"),
            other => panic!("the rotation announced something else: {other:?}"),
        }
        assert!(
            !format!("{:?}", *frame.update).contains("next"),
            "the rotated token is a secret and the Debug line redacts it",
        );
        server.await.expect("server");
    }

    /// A 401 tries the refresh token, and the retry uses the refreshed token.
    #[tokio::test]
    async fn a_401_refreshes_and_retries() {
        let (address, server) = serve(vec![
            response("401 Unauthorized", ""),
            response("200 OK", r#"{"access_token":"fresh","refresh_token":"next"}"#),
            response("200 OK", r#"{"profile":{},"folders":[],"ciphers":[]}"#),
        ])
        .await;
        let session = session_for(address);

        let sync = session.sync().await.expect("the refresh recovers the call");

        assert!(sync.ciphers.is_empty());
        {
            let state = session.state.read();
            let tokens = state.tokens.as_ref().expect("tokens survive");
            assert_eq!(tokens.access, "fresh");
            assert_eq!(tokens.refresh.as_deref(), Some("next"));
        }
        server.await.expect("server");
    }

    /// Two concurrent 401s redeem the rotating refresh token once: the second
    /// reads the access token the first stored instead of spending it again.
    /// The server answers exactly one request, so a second would fail.
    #[tokio::test]
    async fn concurrent_refreshes_spend_the_token_once() {
        let (address, server) =
            serve(vec![response("200 OK", r#"{"access_token":"fresh","refresh_token":"next"}"#)])
                .await;
        let session = session_for(address);

        let (first, second) = tokio::join!(session.refresh("stale"), session.refresh("stale"));

        assert_eq!(first.expect("the first refresh succeeds").as_str(), "fresh");
        assert_eq!(second.expect("the second reads the fresh token").as_str(), "fresh");
        {
            let state = session.state.read();
            let tokens = state.tokens.as_ref().expect("tokens survive");
            assert_eq!(tokens.access, "fresh");
            assert_eq!(tokens.refresh.as_deref(), Some("next"), "the rotated token is kept");
        }
        server.await.expect("the single response was taken");
    }

    /// A rejected refresh locks the session, so the next held call asks for
    /// the master password rather than repeating the 401.
    #[tokio::test]
    async fn a_rejected_refresh_locks_the_session() {
        let (address, server) = serve(vec![
            response("401 Unauthorized", ""),
            response("400 Bad Request", r#"{"error":"invalid_grant"}"#),
        ])
        .await;
        let session = session_for(address);

        let error = session.sync().await.expect_err("the refresh fails");

        assert!(session.is_locked(), "the failed refresh locks the session: {error}");
        assert!(session.state.read().tokens.is_none(), "the tokens are dropped");
        server.await.expect("server");
    }

    /// A session with tokens but no refresh token locks on the first 401
    /// rather than returning Locked while still reading unlocked.
    #[tokio::test]
    async fn a_session_without_a_refresh_token_locks_on_401() {
        let (address, server) = serve(vec![response("401 Unauthorized", "")]).await;
        let session = test_session(
            &VaultConfig { url: format!("http://{address}"), email: "hi@vedhavyas.dev".to_owned() },
            None,
            Path::new("/tmp"),
        )
        .expect("a session");
        session.state.write().tokens = Some(Tokens { access: "stale".to_owned(), refresh: None });

        let error = session.sync().await.expect_err("no refresh token");

        assert!(matches!(error, VaultError::Locked));
        assert!(session.is_locked(), "a missing refresh token locks the session");
        server.await.expect("server");
    }

    #[test]
    fn an_audit_line_carries_the_decision_and_no_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "hi@vedhavyas.dev".to_owned(),
        };
        let config_dir = Path::new("/tmp");
        let session = test_session(&config, Some(dir.path()), config_dir).expect("session");
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
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("audit json");
        let object = value.as_object().expect("an audit line is an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["at", "caller", "decision", "field", "item_id", "item_name", "result", "tool"],
            "the line carries the decision's facts and no value field",
        );
        assert_eq!(value["decision"], "approve");
        assert_eq!(value["tool"], "vault__get");
        assert_eq!(value["field"], "password");
        assert_eq!(value["item_id"], "item-id");
    }

    /// An existing audit file with looser permissions is repaired on open,
    /// which `mode(0o600)` alone does not do.
    #[test]
    fn the_audit_file_is_private_even_when_it_existed_looser() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).expect("dir");
        let path = vault.join("audit-store.jsonl");
        std::fs::write(&path, "").expect("file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let _audit = VaultAudit::new(dir.path(), "store").expect("audit");

        assert_eq!(
            std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600,
            "the audit file is private",
        );
    }

    /// A symlink planted at the audit path is refused rather than followed, so
    /// a line cannot be appended through it.
    #[test]
    fn the_audit_refuses_a_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).expect("dir");
        let target = dir.path().join("target");
        std::fs::write(&target, "").expect("target");
        symlink(&target, vault.join("audit-store.jsonl")).expect("symlink");

        assert!(VaultAudit::new(dir.path(), "store").is_err(), "the symlink is not followed");
        assert_eq!(std::fs::read(&target).expect("target read"), b"");
    }

    /// A decryption failure under a known key means the account rotated them:
    /// the session drops them and the next held call asks for the password.
    #[test]
    fn a_decrypt_failure_drops_the_keys() {
        let session = test_session(&config(), None, Path::new("/tmp")).expect("a session");
        session.state.write().keys = Some(std::sync::Arc::new(Unlocked {
            user_key: crypto::make_symmetric_key(),
            private_key: None,
            org_keys: std::collections::HashMap::new(),
            failed_orgs: Vec::new(),
            user_key_fingerprint: String::new(),
        }));
        assert!(!session.is_locked());

        session.note_decrypt_failure(&SessionSlot::lead("Default", "forge"), None);

        assert!(session.is_locked(), "the stale keys are dropped");
        assert!(matches!(session.keys(), Err(VaultError::Locked)));
    }

    #[test]
    fn the_audit_rotates_through_six_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let audit = VaultAudit::new(dir.path(), "store").expect("audit");
        // Write enough lines to force one rotation.
        let line = "x".repeat(1024 * 1024);
        for _ in 0..11 {
            audit.append(&line).expect("append");
        }
        let base = dir.path().join("vault");
        assert!(base.join("audit-store.jsonl.1").exists(), "the first rotation made .1");
        for name in ["audit-store.jsonl", "audit-store.jsonl.1"] {
            assert_eq!(
                std::fs::metadata(base.join(name)).expect("metadata").permissions().mode() & 0o777,
                0o600,
                "{name} is private",
            );
        }
    }

    /// The production clients are the ones the vault session uses, and they
    /// refuse a plaintext URL before touching the network.
    #[tokio::test]
    async fn production_client_is_https_only() {
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let client = production_client(&format!("http://{address}")).expect("clients build");
        let error = client.prelogin("hi@example.com").await.expect_err("https only");
        assert!(matches!(error, VaultError::Http(_)), "{error}");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept()).await.is_err(),
            "no plaintext request was made",
        );
    }
}
