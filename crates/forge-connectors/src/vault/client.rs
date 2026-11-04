//! The Vaultwarden HTTP client. Stateless about auth: every authed call takes
//! the access token the workspace's `VaultSession` holds, so the session is the
//! one owner of the tokens and the client can be shared across seats.

use serde::de::DeserializeOwned;
use serde_json::Value;
use zeroize::Zeroize;

use crate::vault::VaultError;
use crate::vault::models::{
    AttachmentMeta, Folder, Prelogin, SyncData, TokenResponse, TwoFactorCode,
};

/// The client version every request carries. `GET /api/sync` omits SSH keys
/// unless this is at least 2024.12.0, so the value is load-bearing rather than
/// decorative.
pub const CLIENT_VERSION: &str = "2026.6.0";

/// The OAuth client id Vaultwarden's token endpoint expects for a password
/// grant. There is no secret: the password grant is a public client.
pub const CLIENT_ID: &str = "cli";

pub struct VaultClient {
    http: reqwest::Client,
    base: String,
    device_id: String,
}

impl VaultClient {
    /// Build a client for `base`, which may carry a trailing slash.
    pub fn new(base: &str) -> Result<Self, VaultError> {
        let http = reqwest::Client::builder()
            .user_agent(format!("forge-vault/{}", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            device_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    /// The base URL this client talks to.
    pub fn base(&self) -> &str {
        &self.base
    }

    fn unauth(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .header("Bitwarden-Client-Version", CLIENT_VERSION)
    }

    fn authed(&self, method: reqwest::Method, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.unauth(method, path).bearer_auth(token)
    }

    /// Read one response: map a non-2xx status to its named error and parse the
    /// body as JSON. An empty success body is `Value::Null`.
    async fn send_json(&self, resp: reqwest::Response) -> Result<Value, VaultError> {
        let status = resp.status();
        let retry_after = retry_after(&resp);
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(api_error(status.as_u16(), retry_after, safe_error(&body)));
        }
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|e| VaultError::InvalidResponse(e.to_string()))
    }

    async fn send_typed<T: DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, VaultError> {
        let value = self.send_json(resp).await?;
        serde_json::from_value(value).map_err(|e| VaultError::InvalidResponse(e.to_string()))
    }

    /// `POST /identity/accounts/prelogin`, which is where Vaultwarden mounts it
    /// (the `/api/accounts/prelogin` forms answer 404).
    pub async fn prelogin(&self, email: &str) -> Result<Prelogin, VaultError> {
        let resp = self
            .unauth(reqwest::Method::POST, "/identity/accounts/prelogin")
            .json(&serde_json::json!({ "email": email }))
            .send()
            .await?;
        self.send_typed(resp).await
    }

    /// `POST /identity/connect/token` with the password grant.
    pub async fn login_password(
        &self,
        email: &str,
        master_password_hash: &str,
        two_factor: Option<&TwoFactorCode>,
    ) -> Result<TokenResponse, VaultError> {
        let mut form = vec![
            ("grant_type".to_owned(), "password".to_owned()),
            ("client_id".to_owned(), CLIENT_ID.to_owned()),
            ("username".to_owned(), email.to_owned()),
            ("password".to_owned(), master_password_hash.to_owned()),
            ("scope".to_owned(), "api offline_access".to_owned()),
            ("device_type".to_owned(), "8".to_owned()),
            ("device_identifier".to_owned(), self.device_id.clone()),
            ("device_name".to_owned(), "forge".to_owned()),
        ];
        if let Some(code) = two_factor {
            form.push(("two_factor_token".to_owned(), code.token.clone()));
            form.push(("two_factor_provider".to_owned(), code.provider.to_string()));
        }
        let resp = self
            .unauth(reqwest::Method::POST, "/identity/connect/token")
            .form(&form)
            .send()
            .await?;
        for (_, value) in &mut form {
            value.zeroize();
        }
        self.send_typed(resp).await
    }

    /// `POST /identity/connect/token` with the refresh grant.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, VaultError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ];
        let resp = self
            .unauth(reqwest::Method::POST, "/identity/connect/token")
            .form(&form)
            .send()
            .await?;
        self.send_typed(resp).await
    }

    /// `GET /api/sync`: the profile and every cipher and folder.
    pub async fn sync(&self, token: &str) -> Result<SyncData, VaultError> {
        let resp = self.authed(reqwest::Method::GET, "/api/sync", token).send().await?;
        self.send_typed(resp).await
    }

    pub async fn get_cipher(&self, token: &str, id: &str) -> Result<Value, VaultError> {
        let resp =
            self.authed(reqwest::Method::GET, &format!("/api/ciphers/{id}"), token).send().await?;
        self.send_json(resp).await
    }

    pub async fn create_cipher(&self, token: &str, body: &Value) -> Result<Value, VaultError> {
        let resp =
            self.authed(reqwest::Method::POST, "/api/ciphers", token).json(body).send().await?;
        self.send_json(resp).await
    }

    pub async fn update_cipher(
        &self,
        token: &str,
        id: &str,
        body: &Value,
    ) -> Result<Value, VaultError> {
        let resp = self
            .authed(reqwest::Method::PUT, &format!("/api/ciphers/{id}"), token)
            .json(body)
            .send()
            .await?;
        self.send_json(resp).await
    }

    /// The soft delete. `POST /api/ciphers/{id}/delete` and
    /// `DELETE /api/ciphers/{id}` are permanent and are never called here.
    pub async fn trash_cipher(&self, token: &str, id: &str) -> Result<Value, VaultError> {
        let resp = self
            .authed(reqwest::Method::PUT, &format!("/api/ciphers/{id}/delete"), token)
            .send()
            .await?;
        self.send_json(resp).await
    }

    pub async fn restore_cipher(&self, token: &str, id: &str) -> Result<Value, VaultError> {
        let resp = self
            .authed(reqwest::Method::PUT, &format!("/api/ciphers/{id}/restore"), token)
            .send()
            .await?;
        self.send_json(resp).await
    }

    pub async fn list_folders(&self, token: &str) -> Result<Vec<Folder>, VaultError> {
        let resp = self.authed(reqwest::Method::GET, "/api/folders", token).send().await?;
        self.send_typed(resp).await
    }

    pub async fn create_folder(&self, token: &str, name: &str) -> Result<Value, VaultError> {
        let resp = self
            .authed(reqwest::Method::POST, "/api/folders", token)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await?;
        self.send_json(resp).await
    }

    pub async fn update_folder(
        &self,
        token: &str,
        id: &str,
        name: &str,
    ) -> Result<Value, VaultError> {
        let resp = self
            .authed(reqwest::Method::PUT, &format!("/api/folders/{id}"), token)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await?;
        self.send_json(resp).await
    }

    pub async fn delete_folder(&self, token: &str, id: &str) -> Result<Value, VaultError> {
        let resp = self
            .authed(reqwest::Method::DELETE, &format!("/api/folders/{id}"), token)
            .send()
            .await?;
        self.send_json(resp).await
    }

    /// Create the v2 attachment slot, which records the wrapped key and the
    /// declared size the server later checks against the upload.
    pub async fn create_attachment_slot(
        &self,
        token: &str,
        cipher_id: &str,
        body: &Value,
    ) -> Result<Value, VaultError> {
        let resp = self
            .authed(
                reqwest::Method::POST,
                &format!("/api/ciphers/{cipher_id}/attachment/v2"),
                token,
            )
            .json(body)
            .send()
            .await?;
        self.send_json(resp).await
    }

    /// Upload the encrypted blob. Exactly one `data` part: v2 stored the key at
    /// slot time, and a second part leaks multipart boundary bytes into the
    /// stored blob through the server's parser.
    pub async fn upload_attachment(
        &self,
        token: &str,
        cipher_id: &str,
        attachment_id: &str,
        data: Vec<u8>,
    ) -> Result<Value, VaultError> {
        let part = reqwest::multipart::Part::bytes(data)
            .file_name("blob")
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new().part("data", part);
        let resp = self
            .authed(
                reqwest::Method::POST,
                &format!("/api/ciphers/{cipher_id}/attachment/{attachment_id}"),
                token,
            )
            .multipart(form)
            .send()
            .await?;
        self.send_json(resp).await
    }

    /// The download metadata: the wrapped key and the signed URL.
    pub async fn get_attachment_meta(
        &self,
        token: &str,
        cipher_id: &str,
        attachment_id: &str,
    ) -> Result<AttachmentMeta, VaultError> {
        let resp = self
            .authed(
                reqwest::Method::GET,
                &format!("/api/ciphers/{cipher_id}/attachment/{attachment_id}"),
                token,
            )
            .send()
            .await?;
        self.send_typed(resp).await
    }

    /// Delete one attachment. Used only by a replace, which names the old and
    /// the new file in one approval; it is never its own agent-callable tool.
    pub async fn delete_attachment(
        &self,
        token: &str,
        cipher_id: &str,
        attachment_id: &str,
    ) -> Result<Value, VaultError> {
        let resp = self
            .authed(
                reqwest::Method::DELETE,
                &format!("/api/ciphers/{cipher_id}/attachment/{attachment_id}"),
                token,
            )
            .send()
            .await?;
        self.send_json(resp).await
    }

    /// Download a signed attachment URL. The bytes are opaque ciphertext, so the
    /// request asks the proxy not to re-encode them. A relative URL is the API
    /// route and takes the bearer; an absolute one is signed and does not.
    pub async fn download_attachment(&self, token: &str, url: &str) -> Result<Vec<u8>, VaultError> {
        let absolute = url.starts_with("http://") || url.starts_with("https://");
        let target = if absolute { url.to_owned() } else { format!("{}{}", self.base, url) };
        let mut request = self
            .http
            .get(target)
            .header("Bitwarden-Client-Version", CLIENT_VERSION)
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        if !absolute {
            request = request.bearer_auth(token);
        }
        let resp = request.send().await?;
        let status = resp.status();
        let retry_after = retry_after(&resp);
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(api_error(status.as_u16(), retry_after, safe_error(&body)));
        }
        Ok(resp.bytes().await?.to_vec())
    }
}

/// The server's `Retry-After` hint, when it sends a whole number of seconds.
fn retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// The server's own error field and nothing else. A proxy that echoes the
/// request body must not put the master-password hash into a tool result or
/// the audit file; the token endpoint is the reason this exists.
fn safe_error(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else { return String::new() };
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("error_description").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

/// Map a non-2xx response to the error the facade branches on.
fn api_error(status: u16, retry_after: Option<u64>, body: String) -> VaultError {
    match status {
        401 => VaultError::Unauthorized,
        403 => VaultError::Forbidden(body),
        404 => VaultError::NotFound(body),
        429 => VaultError::RateLimited { retry_after },
        other => VaultError::Api { status: other, body },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read]);
            if let Some(end) = find(&buf, b"\r\n\r\n") {
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

    async fn serve_once(
        status: &str,
        body: &str,
        headers: &str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n{headers}\r\n{body}",
            body.len()
        );
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let _ = socket.write_all(response.as_bytes()).await;
            request
        });
        (address, handle)
    }

    #[tokio::test]
    async fn prelogin_posts_to_the_identity_path_with_the_email() {
        let (address, server) =
            serve_once("200 OK", r#"{"kdf":0,"kdfIterations":1000000}"#, "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        let pre = client.prelogin("hi@vedhavyas.dev").await.unwrap();

        assert_eq!(pre.kdf, 0);
        assert_eq!(pre.kdf_iterations, 1_000_000);
        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("POST /identity/accounts/prelogin "), "{text}");
        assert!(text.contains("hi@vedhavyas.dev"), "{text}");
        assert!(text.contains("bitwarden-client-version: 2026.6.0"), "{text}");
    }

    /// The trash verb is the soft delete and must stay a PUT to `/delete`; the
    /// POST and DELETE forms are permanent.
    #[tokio::test]
    async fn trash_is_a_put_to_the_soft_delete_path() {
        let (address, server) = serve_once("200 OK", "{}", "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        client.trash_cipher("token", "cipher-id").await.unwrap();

        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("PUT /api/ciphers/cipher-id/delete "), "{text}");
        assert!(text.contains("authorization: Bearer token"), "{text}");
    }

    #[tokio::test]
    async fn a_401_is_the_unauthorized_refresh_trigger() {
        let (address, server) = serve_once("401 Unauthorized", "{}", "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        let error = client.sync("stale").await.unwrap_err();

        assert!(matches!(error, VaultError::Unauthorized));
        server.await.unwrap();
    }

    /// A proxy echoing the token request must not put the password hash in a
    /// tool result or the audit file; only the server's error field survives.
    #[tokio::test]
    async fn a_token_error_keeps_the_error_field_and_not_the_body() {
        let (address, server) =
            serve_once("400 Bad Request", r#"{"error":"invalid_grant","password":"hunter2"}"#, "")
                .await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        let error =
            client.login_password("hi@vedhavyas.dev", "the-password-hash", None).await.unwrap_err();
        let text = error.to_string();

        assert!(text.contains("invalid_grant"), "{text}");
        assert!(!text.contains("hunter2"), "the raw body reached the error: {text}");
        server.await.unwrap();
    }

    /// A relative download is the API route and takes the bearer; an absolute
    /// one is signed and must not.
    #[tokio::test]
    async fn a_relative_download_carries_the_bearer_and_an_absolute_one_does_not() {
        let (address, server) = serve_once("200 OK", "bytes", "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();
        let downloaded = client.download_attachment("token", "/attachments/c/a").await.unwrap();
        assert_eq!(downloaded, b"bytes");
        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.contains("authorization: Bearer token"), "{text}");
        assert!(text.contains("accept-encoding: identity"), "{text}");

        let (address, server) = serve_once("200 OK", "bytes", "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();
        let signed = format!("http://{address}/signed?token=x");
        client.download_attachment("token", &signed).await.unwrap();
        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(
            !text.to_lowercase().contains("authorization:"),
            "a signed URL carries its own token: {text}",
        );
    }

    #[tokio::test]
    async fn a_429_carries_the_retry_after_hint() {
        let (address, server) =
            serve_once("429 Too Many Requests", "{}", "Retry-After: 7\r\n").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        let error = client.sync("token").await.unwrap_err();

        assert!(matches!(error, VaultError::RateLimited { retry_after: Some(7) }));
        server.await.unwrap();
    }

    /// The v2 upload sends exactly one `data` part. A second part leaks
    /// boundary bytes into the stored blob through the live hop.
    #[tokio::test]
    async fn the_v2_upload_sends_one_data_part() {
        let (address, server) = serve_once("200 OK", "{}", "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();
        let blob: Vec<u8> = (0u8..97).collect();

        client
            .upload_attachment("token", "cipher-id", "attachment-id", blob.clone())
            .await
            .unwrap();

        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert_eq!(text.matches("name=\"data\"").count(), 1, "{text}");
        assert!(!text.contains("name=\"key\""), "{text}");
        assert!(
            request.windows(blob.len()).any(|window| window == blob.as_slice()),
            "the multipart request did not carry the blob bytes unmodified"
        );
    }
}
