//! The Vaultwarden HTTP client. Stateless about auth: every authed call takes
//! the access token the workspace's `VaultSession` holds, so the session is the
//! one owner of the tokens and the client can be shared across seats.

use std::time::Duration;

use futures::StreamExt;
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

/// Bounds for one ordinary vault call. A stalled server must fail the call
/// rather than hold the unlock or refresh gate for ever, which would queue
/// every later vault call behind it.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const READ_TIMEOUT: Duration = Duration::from_secs(60);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// A download is opaque ciphertext up to the attachment cap, so its idle read
/// bound and total bound are longer than an API call's.
pub const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The user agent every vault request carries.
pub fn user_agent() -> String {
    format!("forge-vault/{}", env!("CARGO_PKG_VERSION"))
}

pub struct VaultClient {
    http: reqwest::Client,
    download_http: reqwest::Client,
    base: String,
    base_url: url::Url,
    device_id: String,
}

impl VaultClient {
    /// Build a client for `base`, which may carry a trailing slash. The
    /// workspace's production session builds its clients through
    /// [`VaultClient::with_clients`] so the machine's TLS roots and
    /// `https_only` apply; this constructor is for local servers and tests.
    pub fn new(base: &str) -> Result<Self, VaultError> {
        let http = reqwest::Client::builder()
            .user_agent(user_agent())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        let download_http = reqwest::Client::builder()
            .user_agent(user_agent())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(DOWNLOAD_READ_TIMEOUT)
            .timeout(DOWNLOAD_TIMEOUT)
            .build()?;
        Self::with_clients(base, http, download_http)
    }

    /// Build a client over callers' own HTTP clients. The download client is
    /// separate because its read and total bounds are longer than an API
    /// call's.
    pub fn with_clients(
        base: &str,
        http: reqwest::Client,
        download_http: reqwest::Client,
    ) -> Result<Self, VaultError> {
        let base = base.trim_end_matches('/').to_owned();
        let base_url = url::Url::parse(&base).map_err(|error| {
            VaultError::InvalidInput(format!("the vault URL does not parse: {error}"))
        })?;
        Ok(Self {
            http,
            download_http,
            base,
            base_url,
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

    /// Read one response body, mapping a non-2xx status to its named error.
    /// The body is zeroized on every path: a token response's body carries
    /// the access and refresh tokens.
    async fn send_body(
        &self,
        resp: reqwest::Response,
    ) -> Result<zeroize::Zeroizing<String>, VaultError> {
        let status = resp.status();
        let retry_after = retry_after(&resp);
        let body = zeroize::Zeroizing::new(
            resp.text().await.map_err(|error| VaultError::Http(error.without_url()))?,
        );
        if !status.is_success() {
            return Err(api_error(status.as_u16(), retry_after, safe_error(&body)));
        }
        Ok(body)
    }

    /// Read one response as JSON. An empty success body is `Value::Null`.
    async fn send_json(&self, resp: reqwest::Response) -> Result<Value, VaultError> {
        let body = self.send_body(resp).await?;
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|e| VaultError::InvalidResponse(e.to_string()))
    }

    /// Parse one response straight into `T`, zeroizing the raw text after the
    /// parse rather than leaving a token-bearing `Value` to drop unzeroized.
    async fn send_typed<T: DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, VaultError> {
        let body = self.send_body(resp).await?;
        serde_json::from_str(&body).map_err(|e| VaultError::InvalidResponse(e.to_string()))
    }

    /// `POST /identity/accounts/prelogin`, which is where Vaultwarden mounts it
    /// (the `/api/accounts/prelogin` forms answer 404).
    pub async fn prelogin(&self, email: &str) -> Result<Prelogin, VaultError> {
        let request = self
            .unauth(reqwest::Method::POST, "/identity/accounts/prelogin")
            .json(&serde_json::json!({ "email": email }));
        let resp = send_request(request).await?;
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
        let request = self.unauth(reqwest::Method::POST, "/identity/connect/token").form(&form);
        let sent = send_request(request).await;
        for (_, value) in &mut form {
            value.zeroize();
        }
        self.send_typed(sent?).await
    }

    /// `POST /identity/connect/token` with the refresh grant.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, VaultError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ];
        let request = self.unauth(reqwest::Method::POST, "/identity/connect/token").form(&form);
        let resp = send_request(request).await?;
        self.send_typed(resp).await
    }

    /// `GET /api/sync`: the profile and every cipher and folder.
    pub async fn sync(&self, token: &str) -> Result<SyncData, VaultError> {
        let request = self.authed(reqwest::Method::GET, "/api/sync", token);
        let resp = send_request(request).await?;
        self.send_typed(resp).await
    }

    pub async fn get_cipher(&self, token: &str, id: &str) -> Result<Value, VaultError> {
        let path = format!("/api/ciphers/{}", path_segment(id)?);
        let request = self.authed(reqwest::Method::GET, &path, token);
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    pub async fn create_cipher(&self, token: &str, body: &Value) -> Result<Value, VaultError> {
        let request = self.authed(reqwest::Method::POST, "/api/ciphers", token).json(body);
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    pub async fn update_cipher(
        &self,
        token: &str,
        id: &str,
        body: &Value,
    ) -> Result<Value, VaultError> {
        let path = format!("/api/ciphers/{}", path_segment(id)?);
        let request = self.authed(reqwest::Method::PUT, &path, token).json(body);
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    /// The soft delete. `POST /api/ciphers/{id}/delete` and
    /// `DELETE /api/ciphers/{id}` are permanent and are never called here.
    pub async fn trash_cipher(&self, token: &str, id: &str) -> Result<Value, VaultError> {
        let path = format!("/api/ciphers/{}/delete", path_segment(id)?);
        let request = self.authed(reqwest::Method::PUT, &path, token);
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    pub async fn restore_cipher(&self, token: &str, id: &str) -> Result<Value, VaultError> {
        let path = format!("/api/ciphers/{}/restore", path_segment(id)?);
        let request = self.authed(reqwest::Method::PUT, &path, token);
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    pub async fn list_folders(&self, token: &str) -> Result<Vec<Folder>, VaultError> {
        let request = self.authed(reqwest::Method::GET, "/api/folders", token);
        let resp = send_request(request).await?;
        self.send_typed(resp).await
    }

    pub async fn create_folder(&self, token: &str, name: &str) -> Result<Value, VaultError> {
        let request = self
            .authed(reqwest::Method::POST, "/api/folders", token)
            .json(&serde_json::json!({ "name": name }));
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    pub async fn update_folder(
        &self,
        token: &str,
        id: &str,
        name: &str,
    ) -> Result<Value, VaultError> {
        let path = format!("/api/folders/{}", path_segment(id)?);
        let request = self
            .authed(reqwest::Method::PUT, &path, token)
            .json(&serde_json::json!({ "name": name }));
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    pub async fn delete_folder(&self, token: &str, id: &str) -> Result<Value, VaultError> {
        let path = format!("/api/folders/{}", path_segment(id)?);
        let request = self.authed(reqwest::Method::DELETE, &path, token);
        let resp = send_request(request).await?;
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
        let path = format!("/api/ciphers/{}/attachment/v2", path_segment(cipher_id)?);
        let request = self.authed(reqwest::Method::POST, &path, token).json(body);
        let resp = send_request(request).await?;
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
        data: &[u8],
    ) -> Result<Value, VaultError> {
        let part = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name("blob")
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new().part("data", part);
        let path = format!(
            "/api/ciphers/{}/attachment/{}",
            path_segment(cipher_id)?,
            path_segment(attachment_id)?
        );
        let request = self.authed(reqwest::Method::POST, &path, token).multipart(form);
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    /// The download metadata: the wrapped key and the signed URL.
    pub async fn get_attachment_meta(
        &self,
        token: &str,
        cipher_id: &str,
        attachment_id: &str,
    ) -> Result<AttachmentMeta, VaultError> {
        let path = format!(
            "/api/ciphers/{}/attachment/{}",
            path_segment(cipher_id)?,
            path_segment(attachment_id)?
        );
        let request = self.authed(reqwest::Method::GET, &path, token);
        let resp = send_request(request).await?;
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
        let path = format!(
            "/api/ciphers/{}/attachment/{}",
            path_segment(cipher_id)?,
            path_segment(attachment_id)?
        );
        let request = self.authed(reqwest::Method::DELETE, &path, token);
        let resp = send_request(request).await?;
        self.send_json(resp).await
    }

    /// Download a signed attachment URL. The bytes are opaque ciphertext, so the
    /// request asks the proxy not to re-encode them. A relative URL is the API
    /// route and takes the bearer; an absolute one is signed and does not, and
    /// may only name this vault's own host. `max_bytes` bounds the body while it
    /// streams, so an endless response cannot buffer past the cap.
    pub async fn download_attachment(
        &self,
        token: &str,
        url: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, VaultError> {
        let absolute = url.starts_with("http://") || url.starts_with("https://");
        // A leading slash is this vault's own path, so appending it cannot
        // change the authority. Anything else is resolved by the URL parser:
        // joined as a path, never concatenated as `base` followed by userinfo.
        let target = if url.starts_with('/') {
            url::Url::parse(&format!("{}{}", self.base, url))
                .map_err(|error| VaultError::InvalidUrl(error.to_string()))?
        } else if absolute {
            url::Url::parse(url).map_err(|error| VaultError::InvalidUrl(error.to_string()))?
        } else {
            self.base_url.join(url).map_err(|error| VaultError::InvalidUrl(error.to_string()))?
        };
        if target.scheme() != self.base_url.scheme()
            || target.host_str() != self.base_url.host_str()
            || target.port_or_known_default() != self.base_url.port_or_known_default()
        {
            return Err(VaultError::InvalidUrl(
                "the attachment URL points at a different host".to_owned(),
            ));
        }
        let mut request = self
            .download_http
            .get(target)
            .header("Bitwarden-Client-Version", CLIENT_VERSION)
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        if !absolute {
            request = request.bearer_auth(token);
        }
        let resp = send_request(request).await?;
        let status = resp.status();
        let retry_after = retry_after(&resp);
        if !status.is_success() {
            let body = resp.text().await.map_err(|error| VaultError::Http(error.without_url()))?;
            return Err(api_error(status.as_u16(), retry_after, safe_error(&body)));
        }
        if let Some(declared) = resp.content_length()
            && declared > max_bytes
        {
            return Err(VaultError::AttachmentTooLarge { size: declared, cap: max_bytes });
        }
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| VaultError::Http(error.without_url()))?;
            let size = body.len() as u64 + chunk.len() as u64;
            if size > max_bytes {
                return Err(VaultError::AttachmentTooLarge { size, cap: max_bytes });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// Percent-encode one id for a path segment. An empty id is refused rather
/// than sent as a route with a hole in it.
fn path_segment(id: &str) -> Result<String, VaultError> {
    if id.trim().is_empty() {
        return Err(VaultError::InvalidInput("an empty id was passed".to_owned()));
    }
    // `byte_serialize` leaves dots alone, and a `.` or `..` segment is a
    // traversal rather than an id once a router normalizes the path.
    if id == "." || id == ".." {
        return Err(VaultError::InvalidInput(format!("'{id}' is not an id")));
    }
    Ok(url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>().replace('+', "%20"))
}

/// Send one request, dropping the URL from any transport error: a signed
/// download URL carries its token in the query string.
async fn send_request(request: reqwest::RequestBuilder) -> Result<reqwest::Response, VaultError> {
    request.send().await.map_err(|error| VaultError::Http(error.without_url()))
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

    async fn serve_raw(response: Vec<u8>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _request = read_request(&mut socket).await;
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        });
        (address, handle)
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
        let downloaded =
            client.download_attachment("token", "/attachments/c/a", 1024).await.unwrap();
        assert_eq!(downloaded, b"bytes");
        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.contains("authorization: Bearer token"), "{text}");
        assert!(text.contains("accept-encoding: identity"), "{text}");

        let (address, server) = serve_once("200 OK", "bytes", "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();
        let signed = format!("http://{address}/signed?token=x");
        client.download_attachment("token", &signed, 1024).await.unwrap();
        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(
            !text.to_lowercase().contains("authorization:"),
            "a signed URL carries its own token: {text}",
        );
    }

    /// `base@host` is userinfo plus a foreign host when the base and the URL
    /// are concatenated. Resolved as a path, it stays this vault's route.
    #[tokio::test]
    async fn a_relative_url_cannot_smuggle_the_bearer_to_another_host() {
        let (address, server) = serve_once("200 OK", "bytes", "").await;
        let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other = elsewhere.local_addr().unwrap();
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        let downloaded =
            client.download_attachment("token", &format!("@{other}/x"), 1024).await.unwrap();

        assert_eq!(downloaded, b"bytes");
        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with(&format!("GET /@{other}/x ")), "{text}");
        assert!(text.contains("authorization: Bearer token"), "{text}");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), elsewhere.accept())
                .await
                .is_err(),
            "no request reached the foreign host",
        );
    }

    /// An absolute URL naming another host is refused before any bearer is
    /// attached or any request is sent.
    #[tokio::test]
    async fn a_download_to_another_host_is_refused() {
        let (address, _server) = serve_once("200 OK", "bytes", "").await;
        // A port that was bound and released refuses quickly if the guard is
        // gone, so this fails on its own assertion rather than hanging.
        let closed = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap()
        };
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        // Same host as the base, different port: `Url::join` accepts it, so
        // only the client's own host/port check can refuse it.
        let error = client
            .download_attachment("token", &format!("http://{closed}/x"), 1024)
            .await
            .unwrap_err();

        assert!(matches!(error, VaultError::InvalidUrl(_)), "{error}");
    }

    /// The declared length is checked before the body is read, and the stream
    /// is bounded as it arrives for a response that declares no length.
    #[tokio::test]
    async fn a_download_over_the_cap_is_refused() {
        let (address, server) = serve_once("200 OK", "far more than ten bytes", "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        let error = client.download_attachment("token", "/a", 10).await.unwrap_err();

        assert!(
            matches!(error, VaultError::AttachmentTooLarge { size, cap: 10 } if size > 10),
            "{error}"
        );
        server.await.unwrap();
    }

    /// A response that declares no length is bounded as it arrives, which is
    /// the property the pre-check cannot provide.
    #[tokio::test]
    async fn a_chunked_download_over_the_cap_is_refused_while_streaming() {
        let response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n8\r\nxxxxxxxx\r\n0\r\n\r\n"
                .to_vec();
        let (address, server) = serve_raw(response).await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        let error = client.download_attachment("token", "/a", 4).await.unwrap_err();

        assert!(matches!(error, VaultError::AttachmentTooLarge { .. }), "{error}");
        server.await.unwrap();
    }

    /// A body that fails mid-read carries the failure class and not the URL.
    #[tokio::test]
    async fn a_body_error_does_not_carry_the_url() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/json\r\n\r\n{\"a\":"
                .to_vec();
        let (address, server) = serve_raw(response).await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        let error = client.sync("token").await.unwrap_err();
        let text = error.to_string();

        assert!(matches!(&error, VaultError::Http(_)), "{error}");
        assert!(!text.contains("127.0.0.1"), "the URL reached the error: {text}");
        server.await.unwrap();
    }

    /// A `.` or `..` segment is a traversal once a router normalizes it, not
    /// an id, and `byte_serialize` would leave it untouched.
    #[tokio::test]
    async fn a_dot_segment_id_is_refused() {
        let client = VaultClient::new("http://127.0.0.1:1").unwrap();

        for id in [".", ".."] {
            let error = client.get_cipher("token", id).await.unwrap_err();
            assert!(
                matches!(error, VaultError::InvalidInput(_)),
                "'{id}' must be refused: {error}"
            );
        }
    }

    /// A signed URL's token lives in its query string, so no transport error
    /// may print the URL it was sent to.
    #[tokio::test]
    async fn a_download_error_does_not_carry_the_signed_url() {
        let client = VaultClient::new("http://127.0.0.1:1").unwrap();

        let error = client
            .download_attachment("token", "http://127.0.0.1:1/signed?token=secret", 1024)
            .await
            .unwrap_err();
        let text = error.to_string();

        assert!(!text.contains("secret"), "the signed URL reached the error: {text}");
        assert!(!text.contains("127.0.0.1"), "the URL reached the error: {text}");
    }

    /// The same holds for an ordinary call's transport error.
    #[tokio::test]
    async fn a_send_error_does_not_carry_the_url() {
        let client = VaultClient::new("http://127.0.0.1:1").unwrap();

        let error = client.sync("token").await.unwrap_err();
        let text = error.to_string();

        assert!(!text.contains("127.0.0.1"), "the URL reached the error: {text}");
    }

    /// An id is one path segment: a slash, a query marker or a space in it
    /// cannot change the route the client asks for.
    #[tokio::test]
    async fn an_id_is_percent_encoded_as_one_path_segment() {
        let (address, server) = serve_once("200 OK", "{}", "").await;
        let client = VaultClient::new(&format!("http://{address}")).unwrap();

        client.get_cipher("token", "a/b?c d").await.unwrap();

        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("GET /api/ciphers/a%2Fb%3Fc%20d "), "{text}");
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

        client.upload_attachment("token", "cipher-id", "attachment-id", &blob).await.unwrap();

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
