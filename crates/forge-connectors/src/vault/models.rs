//! The Vaultwarden wire shapes the connector reads and writes.
//!
//! Ciphers cross as `serde_json::Value`, not as a lossy typed struct: an update
//! must send the whole object back, and a typed shape that dropped the TOTP
//! seed, a passkey or a field this version does not model would delete it on
//! the server. The facade decrypts the fields it needs from the value; the
//! connector only moves it.

use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

/// The prelogin answer: which KDF the account uses and its parameters.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Prelogin {
    /// `0` is PBKDF2-SHA256, `1` is Argon2id.
    pub kdf: i32,
    pub kdf_iterations: u32,
    #[serde(default)]
    pub kdf_memory: Option<u32>,
    #[serde(default)]
    pub kdf_parallelism: Option<u32>,
}

/// The token endpoint's answer for both the password and refresh grants.
#[derive(Clone, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// The wrapped user key. Vaultwarden capitalises the field; some clients
    /// lower-case it, so both spellings parse.
    pub key: Option<String>,
    /// The wrapped account private key, in the same two spellings.
    pub private_key: Option<String>,
}

/// Hand-written so a malformed response drops every field it had already
/// read through `Zeroizing`: the access and refresh tokens are credentials.
impl<'de> Deserialize<'de> for TokenResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TokenVisitor;

        impl<'de> Visitor<'de> for TokenVisitor {
            type Value = TokenResponse;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a token response")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut access: Option<Zeroizing<String>> = None;
                let mut refresh: Option<Zeroizing<String>> = None;
                let mut key: Option<Zeroizing<String>> = None;
                let mut private_key: Option<Zeroizing<String>> = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "access_token" => access = Some(Zeroizing::new(map.next_value()?)),
                        "refresh_token" => {
                            refresh = map.next_value::<Option<String>>()?.map(Zeroizing::new);
                        }
                        "Key" | "key" => {
                            key = map.next_value::<Option<String>>()?.map(Zeroizing::new);
                        }
                        "PrivateKey" | "privateKey" => {
                            private_key = map.next_value::<Option<String>>()?.map(Zeroizing::new);
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                let mut access = access
                    .ok_or_else(|| <M::Error as serde::de::Error>::missing_field("access_token"))?;
                Ok(TokenResponse {
                    access_token: std::mem::take(&mut *access),
                    refresh_token: refresh.map(|value| (*value).clone()),
                    key: key.map(|value| (*value).clone()),
                    private_key: private_key.map(|value| (*value).clone()),
                })
            }
        }

        deserializer.deserialize_map(TokenVisitor)
    }
}

/// Every field is a credential, so dropping the token wipes it even on an
/// error path that never stored it.
impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(refresh) = &mut self.refresh_token {
            refresh.zeroize();
        }
        if let Some(key) = &mut self.key {
            key.zeroize();
        }
        if let Some(private_key) = &mut self.private_key {
            private_key.zeroize();
        }
    }
}

/// Hand-written because every field is a credential: the access and refresh
/// tokens, the wrapped user key and the wrapped private key.
impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "[redacted]"))
            .field("key", &self.key.as_ref().map(|_| "[redacted]"))
            .field("private_key", &self.private_key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// The account profile inside `GET /api/sync`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    #[serde(default, rename = "key", alias = "Key")]
    pub key: Option<String>,
    #[serde(default, rename = "privateKey", alias = "PrivateKey")]
    pub private_key: Option<String>,
    #[serde(default)]
    pub organizations: Vec<Organization>,
    #[serde(default)]
    pub email: Option<String>,
}

/// One organization's wrapped key, as the profile carries it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Organization {
    pub id: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// One folder. Folders are simple enough to type: an update sends only the name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: String,
    pub name: String,
}

/// One attachment's metadata, as a cipher and the download route carry it.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentMeta {
    pub id: String,
    pub file_name: String,
    /// Vaultwarden sends the size as a string on some routes and a number on
    /// others; both spellings mean the same byte count.
    #[serde(default, deserialize_with = "size_from_number_or_string")]
    pub size: Option<u64>,
    #[serde(default)]
    pub size_name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// The wrapped attachment key. Never returned past this module.
    #[serde(default)]
    pub key: Option<String>,
}

/// Hand-written because the signed URL carries its own token and the key is a
/// wrapped credential.
impl std::fmt::Debug for AttachmentMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentMeta")
            .field("id", &self.id)
            .field("file_name", &self.file_name)
            .field("size", &self.size)
            .field("size_name", &self.size_name)
            .field("url", &self.url.as_ref().map(|_| "[redacted]"))
            .field("key", &self.key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// The parts of `GET /api/sync` this family reads.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncData {
    #[serde(default)]
    pub profile: Profile,
    #[serde(default)]
    pub folders: Vec<Folder>,
    /// The ciphers stay raw so an update can send the whole object back
    /// without dropping a field this version does not model.
    #[serde(default)]
    pub ciphers: Vec<serde_json::Value>,
}

/// Read an attachment size that arrives as either a JSON number or a string.
fn size_from_number_or_string<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Number(number)) => number.as_u64(),
        Some(serde_json::Value::String(size)) => size.trim().parse().ok(),
        _ => None,
    })
}

/// The second factor a password grant carries, when the account asks for one.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TwoFactorCode {
    pub token: String,
    /// The provider id Vaultwarden expects: `0` is an authenticator app.
    pub provider: i32,
}

/// Hand-written because the token is a credential.
impl std::fmt::Debug for TwoFactorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoFactorCode")
            .field("token", &"[redacted]")
            .field("provider", &self.provider)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signed URL carries its own token and the key is a wrapped
    /// credential, so neither may appear in a Debug line.
    #[test]
    fn attachment_meta_debug_redacts_the_url_and_key() {
        let meta = AttachmentMeta {
            id: "attachment-id".to_owned(),
            file_name: "file".to_owned(),
            size: Some(97),
            size_name: None,
            url: Some("https://vault.example.com/signed?token=secret".to_owned()),
            key: Some("2.c2VjcmV0".to_owned()),
        };

        let text = format!("{meta:?}");

        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("2.c2VjcmV0"), "{text}");
    }
}
