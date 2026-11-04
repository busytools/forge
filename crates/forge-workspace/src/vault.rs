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
use zeroize::{Zeroize, Zeroizing};

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
    /// Serializes refreshes. A rotating refresh token is spent once, so two
    /// concurrent 401s must not both redeem it; the second reads the token
    /// the first stored.
    refresh_gate: tokio::sync::Mutex<()>,
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
        // `bitwarden-crypto`'s `MasterKey` has no public zeroize; it is
        // dropped as soon as the user and private keys are unwrapped.
        let master_key = crypto::derive_master_key(password, &self.email, &prelogin)
            .map_err(VaultError::Crypto)?;
        let hash = Zeroizing::new(crypto::master_password_hash(&master_key, password));
        let mut two_factor =
            two_factor.map(|token| TwoFactorCode { token: token.to_owned(), provider: 0 });
        let token: TokenResponse =
            self.client.login_password(&self.email, &hash, two_factor.as_ref()).await?;
        if let Some(code) = &mut two_factor {
            code.token.zeroize();
        }
        let sync = self.client.sync(&token.access_token).await?;
        let unlocked =
            crypto::unlock(&token, &master_key, &sync.profile).map_err(VaultError::Crypto)?;

        let mut state = self.state.write();
        state.tokens = Some(Tokens { access: token.access_token, refresh: token.refresh_token });
        state.keys = Some(std::sync::Arc::new(unlocked));
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
            Ok(token) => {
                let access = Zeroizing::new(token.access_token.clone());
                let next_refresh =
                    token.refresh_token.or_else(|| Some(std::mem::take(&mut refresh)));
                refresh.zeroize();
                if let Some(tokens) = self.state.write().tokens.as_mut() {
                    tokens.access.zeroize();
                    if let Some(mut old) = tokens.refresh.take() {
                        old.zeroize();
                    }
                    tokens.access = token.access_token;
                    tokens.refresh = next_refresh;
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
        data: Vec<u8>,
    ) -> Result<Value, VaultError> {
        let token = self.access_token()?;
        match self.client.upload_attachment(&token, cipher_id, attachment_id, data.clone()).await {
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

    pub(crate) async fn download_attachment(&self, url: &str) -> Result<Vec<u8>, VaultError> {
        let token = self.access_token()?;
        match self.client.download_attachment(&token, url).await {
            Err(VaultError::Unauthorized) => {
                let token = self.refresh(&token).await?;
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
        use std::os::unix::fs::OpenOptionsExt;
        let dir = app_support.join("vault");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("audit-{store_key}.jsonl"));
        let file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path)?;
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
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(rotated_path(path, AUDIT_MAX_FILES));
    for index in (1..AUDIT_MAX_FILES).rev() {
        let from = rotated_path(path, index);
        if from.exists() {
            let _ = std::fs::rename(&from, rotated_path(path, index + 1));
        }
    }
    let _ = std::fs::rename(path, rotated_path(path, 1));
    audit.file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
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
        let session = VaultSession::new(&config, None, Path::new("/tmp")).expect("a session");
        session.state.write().tokens =
            Some(Tokens { access: "stale".to_owned(), refresh: Some("good".to_owned()) });
        session
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
        let session = VaultSession::new(
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
}
