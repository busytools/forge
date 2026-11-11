//! The unlock pipeline and the encryption primitives, on `bitwarden-crypto`.
//!
//! The shapes here are the ones the live spike proved against Vaultwarden
//! 1.37.4: the wrapped user key and private key decrypt to bytes (the private
//! key is PKCS#8 DER, not PEM), the organization key and the RSA-wrapped
//! attachment keys unwrap with RSA-OAEP-SHA1, and an attachment is the
//! `0x02 | IV | MAC | ciphertext` buffer the official clients upload.

use std::collections::HashMap;
use std::num::NonZeroU32;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use bitwarden_crypto::{
    BitwardenLegacyKeyBytes, EncString, HashPurpose, Kdf, KeyDecryptable, KeyEncryptable,
    MasterKey, OctetStreamBytes,
};
use hmac::{Hmac, KeyInit, Mac};
use rsa::pkcs8::DecodePrivateKey;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::vault::VaultError;
use crate::vault::models::{Prelogin, Profile, TokenResponse};

/// The key type an [`Unlocked`] holds, re-exported so a caller building one
/// for a test can name it.
pub use bitwarden_crypto::SymmetricCryptoKey;

/// An unlocked account's keys: the user key every personal cipher inherits,
/// the account private key, and any organization keys the profile carries.
///
/// The caller (the workspace's `VaultSession`) owns these and is responsible
/// for locking them; this type is the plain hand-off from the unlock path.
pub struct Unlocked {
    pub user_key: SymmetricCryptoKey,
    pub private_key: Option<rsa::RsaPrivateKey>,
    pub org_keys: HashMap<String, SymmetricCryptoKey>,
    /// Organizations whose key the profile carries but this unlock could not
    /// unwrap. A cipher in one of them fails with [`VaultError::MissingKey`]
    /// rather than being encrypted or decrypted with the wrong key.
    pub failed_orgs: Vec<String>,
    /// SHA-256 of the wrapped user key this unlock used, so a later sync can
    /// prove the account's keys did not rotate while the session was open.
    pub user_key_fingerprint: String,
}

impl Unlocked {
    /// The key a cipher inherits: its organization's when it carries one, else
    /// the user key. A personal cipher is the common case; an organization
    /// cipher whose key was not unlocked is refused rather than silently
    /// falling back to the user key.
    fn base_key<'a>(
        &'a self,
        cipher: &serde_json::Value,
    ) -> Result<&'a SymmetricCryptoKey, VaultError> {
        match cipher.get("organizationId").and_then(serde_json::Value::as_str) {
            Some(org_id) => {
                self.org_keys.get(org_id).ok_or_else(|| VaultError::MissingKey(org_id.to_owned()))
            }
            None => Ok(&self.user_key),
        }
    }

    /// Decrypt one of a cipher's encrypted fields with the cipher's own key.
    pub fn decrypt_item_str(
        &self,
        cipher: &serde_json::Value,
        enc: &str,
    ) -> Result<String, VaultError> {
        let key = cipher_key(cipher, self.base_key(cipher)?)?;
        decrypt_str(enc, &key).map_err(VaultError::Crypto)
    }

    /// Encrypt one of a cipher's fields with the cipher's own key.
    pub fn encrypt_item_str(
        &self,
        cipher: &serde_json::Value,
        value: &str,
    ) -> Result<String, VaultError> {
        let key = cipher_key(cipher, self.base_key(cipher)?)?;
        encrypt_str(value, &key).map_err(VaultError::Crypto)
    }

    /// Decrypt a folder name or another value encrypted with the user key.
    pub fn decrypt_user_str(&self, enc: &str) -> Result<String> {
        decrypt_str(enc, &self.user_key)
    }

    /// Encrypt a folder name or another value with the user key.
    pub fn encrypt_user_str(&self, value: &str) -> Result<String> {
        encrypt_str(value, &self.user_key)
    }

    /// Prepare one attachment: a fresh key wrapped with the item key, the
    /// encrypted file name, and the `0x02` blob the server stores.
    pub fn prepare_attachment(
        &self,
        cipher: &serde_json::Value,
        name: &str,
        data: &[u8],
    ) -> Result<(String, String, Vec<u8>), VaultError> {
        let item_key = cipher_key(cipher, self.base_key(cipher)?)?;
        let attachment_key = make_symmetric_key();
        let wrapped = wrap_symmetric_key(&attachment_key, &item_key)?;
        let encrypted_name = encrypt_str(name, &item_key)?;
        let blob = encrypt_attachment_blob(data, &attachment_key)?;
        Ok((wrapped, encrypted_name, blob))
    }

    /// Open one downloaded attachment: unwrap its key with the item key and
    /// decrypt the `0x02` blob.
    pub fn open_attachment(
        &self,
        cipher: &serde_json::Value,
        wrapped_key: &str,
        blob: &[u8],
    ) -> Result<Vec<u8>, VaultError> {
        let item_key = cipher_key(cipher, self.base_key(cipher)?)?;
        let key = unwrap_attachment_key(wrapped_key, &item_key, self.private_key.as_ref())?;
        decrypt_attachment_blob(blob, &key).map_err(VaultError::Crypto)
    }
}

/// The KDF a prelogin answer names. An unknown type is an error rather than a
/// guess, because guessing the KDF derives the wrong key.
pub fn kdf_from_prelogin(pre: &Prelogin) -> Result<Kdf> {
    let nz = |value: u32, name: &str| {
        NonZeroU32::new(value).ok_or_else(|| anyhow!("prelogin returned {name} = 0"))
    };
    match pre.kdf {
        0 => Ok(Kdf::PBKDF2 { iterations: nz(pre.kdf_iterations, "kdfIterations")? }),
        1 => Ok(Kdf::Argon2id {
            iterations: nz(pre.kdf_iterations, "kdfIterations")?,
            // Argon2id without its memory and parallelism is not a parameter
            // set; defaulting would derive a key the account does not hold.
            memory: nz(
                pre.kdf_memory.context("prelogin returned kdf = 1 without kdfMemory")?,
                "kdfMemory",
            )?,
            parallelism: nz(
                pre.kdf_parallelism.context("prelogin returned kdf = 1 without kdfParallelism")?,
                "kdfParallelism",
            )?,
        }),
        other => bail!("prelogin returned unknown kdf type {other}"),
    }
}

/// The KDF in one line, for a log line or a refusal message.
pub fn kdf_label(pre: &Prelogin) -> String {
    match pre.kdf {
        0 => format!("PBKDF2-HMAC-SHA256, iterations={}", pre.kdf_iterations),
        1 => format!(
            "Argon2id, iterations={}, memory={} MiB, parallelism={}",
            pre.kdf_iterations,
            pre.kdf_memory.unwrap_or(0),
            pre.kdf_parallelism.unwrap_or(0)
        ),
        other => format!("unknown({other})"),
    }
}

/// Derive the master key from the password and the configured email.
pub fn derive_master_key(password: &str, email: &str, pre: &Prelogin) -> Result<MasterKey> {
    let kdf = kdf_from_prelogin(pre)?;
    MasterKey::derive(password, email, &kdf)
        .map_err(|e| anyhow!("master key derivation failed: {e}"))
}

/// The hash the token endpoint takes as `password`.
pub fn master_password_hash(master_key: &MasterKey, password: &str) -> String {
    String::from(
        master_key.derive_master_key_hash(password.as_bytes(), HashPurpose::ServerAuthorization),
    )
}

/// Unwrap the user key, the account private key and the organization keys.
pub fn unlock(
    token: &TokenResponse,
    master_key: &MasterKey,
    profile: &Profile,
) -> Result<Unlocked> {
    let wrapped_user_key = wrapped_user_key(token, profile)?;
    let user_key: EncString =
        wrapped_user_key.parse().map_err(|e| anyhow!("wrapped user key did not parse: {e}"))?;
    let user_key = master_key.decrypt_user_key(user_key)?;
    with_user_key(token, user_key, profile, &wrapped_user_key)
}

/// Unlock from a user key a client released through its platform
/// authenticator, rather than one derived from the master password.
///
/// The key arrives base64, as the client's enrolment wrapped it. Everything
/// after it is the same journey [`unlock`] makes: the account private key and
/// the organization keys unwrap from the same sync profile, and the
/// fingerprint is the wrapped user key's, which is what proves the account's
/// keys did not rotate while the session was open.
pub fn unlock_with_user_key(
    token: &TokenResponse,
    user_key: &str,
    profile: &Profile,
) -> Result<Unlocked> {
    let user_key = SymmetricCryptoKey::try_from(user_key.to_owned())
        .map_err(|e| anyhow!("released user key did not parse: {e}"))?;
    let wrapped_user_key = wrapped_user_key(token, profile)?;
    with_user_key(token, user_key, profile, &wrapped_user_key)
}

/// The wrapped user key this unlock will report the fingerprint of. An empty
/// string is a field a client left unset, not a wrapped key; it must not
/// shadow the profile's real one.
fn wrapped_user_key(token: &TokenResponse, profile: &Profile) -> Result<String> {
    token
        .key
        .clone()
        .filter(|key| !key.is_empty())
        .or_else(|| profile.key.clone().filter(|key| !key.is_empty()))
        .context("token response and sync profile both lack the wrapped user key")
}

/// The half of an unlock that starts from a known user key.
fn with_user_key(
    token: &TokenResponse,
    user_key: SymmetricCryptoKey,
    profile: &Profile,
    wrapped_user_key: &str,
) -> Result<Unlocked> {
    let user_key_fingerprint = sha256_hex(wrapped_user_key.as_bytes());

    // An empty `PrivateKey` is a client's unset value, not a wrapped key; it
    // must not shadow the profile's real one either.
    let private_key_enc = token
        .private_key
        .clone()
        .filter(|key| !key.is_empty())
        .or_else(|| profile.private_key.clone().filter(|key| !key.is_empty()));
    let private_key = match private_key_enc {
        Some(enc) => {
            let der = Zeroizing::new(
                decrypt_bytes(&enc, &user_key).context("decrypting the private key")?,
            );
            Some(parse_private_key(&der)?)
        }
        None => None,
    };

    let (org_keys, failed_orgs) = org_keys(private_key.as_ref(), profile);

    Ok(Unlocked { user_key, private_key, org_keys, failed_orgs, user_key_fingerprint })
}

/// Unwrap every organization key the profile carries, naming the ones that
/// failed. A failed org is not fatal to the unlock - the account is still
/// usable - but its ciphers must not be touched with another key.
fn org_keys(
    private_key: Option<&rsa::RsaPrivateKey>,
    profile: &Profile,
) -> (HashMap<String, SymmetricCryptoKey>, Vec<String>) {
    let mut keys = HashMap::new();
    let mut failed = Vec::new();
    let Some(private) = private_key else {
        // No private key means no organization key can be unwrapped.
        return (keys, profile.organizations.iter().map(|org| org.id.clone()).collect());
    };
    for org in &profile.organizations {
        let Some(key) = org.key.as_deref().filter(|key| !key.is_empty()) else {
            failed.push(org.id.clone());
            continue;
        };
        match unwrap_org_key(key, private) {
            Ok(unwrapped) => {
                keys.insert(org.id.clone(), unwrapped);
            }
            Err(error) => {
                tracing::warn!(
                    target: "forge_tools::vault",
                    event_name = "vault_org_key_unwrap_failed",
                    org_id = %org.id,
                    error = %error,
                    "an organization key was not unwrapped",
                );
                failed.push(org.id.clone());
            }
        }
    }
    (keys, failed)
}

/// Unwrap an organization key: EncString type 4, RSA-2048-OAEP-SHA1 with the
/// user's private key. Type 6 is named rather than guessed at.
pub fn unwrap_org_key(enc: &str, private: &rsa::RsaPrivateKey) -> Result<SymmetricCryptoKey> {
    let (kind, rest) = enc.split_once('.').context("org key has no EncString header")?;
    match kind {
        "4" => {}
        "6" => bail!("org key is EncString type 6 (OAEP-SHA1 + HMAC); only type 4 is unwrapped"),
        other => bail!("org key is EncString type {other}, not an RSA-OAEP wrap"),
    }
    let parts: Vec<&str> = rest.split('|').collect();
    let data = parts.first().ok_or_else(|| anyhow!("org key has no ciphertext part"))?;
    rsa_oaep_unwrap(data, private, "org key")
}

fn rsa_oaep_unwrap(
    data_b64: &str,
    private: &rsa::RsaPrivateKey,
    what: &str,
) -> Result<SymmetricCryptoKey> {
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .with_context(|| format!("{what} ciphertext is not base64"))?;
    let decrypted = Zeroizing::new(
        private
            .decrypt(rsa::Oaep::<sha1::Sha1>::new(), &ciphertext)
            .map_err(|e| anyhow!("RSA-OAEP unwrap failed: {e}"))?,
    );
    let legacy = BitwardenLegacyKeyBytes::from(decrypted.to_vec());
    SymmetricCryptoKey::try_from(&legacy)
        .map_err(|e| anyhow!("unwrapped {what} is not a symmetric key: {e}"))
}

/// The EncString type byte of a string, without decoding it: `"2...."` gives
/// `Some(2)`.
pub fn enc_string_type(enc: &str) -> Option<u8> {
    enc.split_once('.').and_then(|(kind, _)| kind.parse::<u8>().ok())
}

/// Unwrap an attachment key by its EncString type.
///
/// Types 0/1/2 are AES-wrapped with the item key. Types 4/6 are RSA-OAEP-SHA1
/// wrapped with the user's public key, so they need the private key; type 6
/// also carries an HMAC-SHA256 over `iv | ciphertext` keyed with the item key's
/// MAC half. The live account uses type 2 for all 19 attachments.
pub fn unwrap_attachment_key(
    enc: &str,
    item_key: &SymmetricCryptoKey,
    private_key: Option<&rsa::RsaPrivateKey>,
) -> Result<SymmetricCryptoKey, VaultError> {
    let (kind, rest) = enc.split_once('.').context("attachment key has no EncString header")?;
    match kind {
        "0" | "1" | "2" => {
            let parsed: EncString =
                enc.parse().map_err(|e| anyhow!("attachment key EncString parse failed: {e}"))?;
            let raw = Zeroizing::<Vec<u8>>::new(
                parsed
                    .decrypt_with_key(item_key)
                    .map_err(|e| anyhow!("attachment key decrypt failed: {e}"))?,
            );
            let legacy = BitwardenLegacyKeyBytes::from(raw.to_vec());
            SymmetricCryptoKey::try_from(&legacy).map_err(|e| {
                VaultError::Crypto(anyhow!("unwrapped attachment key is not a symmetric key: {e}"))
            })
        }
        "4" | "6" => {
            // A missing private key is not a decrypt failure: the account has
            // none to unlock, so it gets its own error.
            let private_key = private_key.ok_or(VaultError::MissingPrivateKey)?;
            let parts: Vec<&str> = rest.split('|').collect();
            // `4.<ciphertext>`; `6.<ciphertext>|<mac>`. The MAC covers the
            // ciphertext only, per bitwarden-crypto's UnsignedSharedKey.
            let data = parts.first().context("attachment key has no ciphertext part")?;
            if kind == "6" {
                let mac = parts.get(1).context("attachment key type 6 has no MAC part")?;
                verify_type6_mac(data, mac, item_key)?;
            }
            rsa_oaep_unwrap(data, private_key, "attachment key").map_err(VaultError::Crypto)
        }
        other => Err(VaultError::Crypto(anyhow!(
            "attachment key is EncString type {other}; no unwrap implemented"
        ))),
    }
}

fn verify_type6_mac(data_b64: &str, mac_b64: &str, item_key: &SymmetricCryptoKey) -> Result<()> {
    let decode = |value: &str, what: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .with_context(|| format!("{what} is not base64"))
    };
    let data = decode(data_b64, "type-6 ciphertext")?;
    let mac = decode(mac_b64, "type-6 MAC")?;
    let encoded = item_key.to_encoded();
    let mac_key = encoded.as_ref().get(32..64).context("item key has no MAC half")?;
    let mut hmac =
        Hmac::<sha2::Sha256>::new_from_slice(mac_key).map_err(|e| anyhow!("HMAC init: {e}"))?;
    hmac.update(&data);
    hmac.verify_slice(&mac).map_err(|_| anyhow!("attachment key type-6 MAC did not verify"))
}

pub fn decrypt_str(enc: &str, key: &SymmetricCryptoKey) -> Result<String> {
    let parsed: EncString = enc.parse().map_err(|e| anyhow!("EncString parse failed: {e}"))?;
    parsed.decrypt_with_key(key).map_err(|e| anyhow!("EncString decrypt failed: {e}"))
}

pub fn decrypt_bytes(enc: &str, key: &SymmetricCryptoKey) -> Result<Vec<u8>> {
    let parsed: EncString = enc.parse().map_err(|e| anyhow!("EncString parse failed: {e}"))?;
    parsed.decrypt_with_key(key).map_err(|e| anyhow!("EncString decrypt failed: {e}"))
}

/// The account private key is stored as raw PKCS#8 DER by the Bitwarden
/// clients; older exports and other clients may hold PEM or base64 DER, so
/// every shape is tried.
fn parse_private_key(bytes: &[u8]) -> Result<rsa::RsaPrivateKey> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        if text.trim_start().starts_with("-----BEGIN") {
            return rsa::RsaPrivateKey::from_pkcs8_pem(text)
                .context("parsing the private key as PEM");
        }
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(text.trim())
            && let Ok(key) = rsa::RsaPrivateKey::from_pkcs8_der(&Zeroizing::new(der))
        {
            return Ok(key);
        }
    }
    rsa::RsaPrivateKey::from_pkcs8_der(bytes).context("parsing the private key as PKCS#8 DER")
}

/// A cipher's own key, when the item carries one, else the user/org key it
/// inherits.
pub fn cipher_key(
    cipher: &serde_json::Value,
    base: &SymmetricCryptoKey,
) -> Result<SymmetricCryptoKey> {
    match cipher.get("key").and_then(|v| v.as_str()) {
        Some(enc) if !enc.is_empty() => {
            let parsed: EncString =
                enc.parse().map_err(|e| anyhow!("cipher key EncString parse failed: {e}"))?;
            let raw = Zeroizing::<Vec<u8>>::new(
                parsed
                    .decrypt_with_key(base)
                    .map_err(|e| anyhow!("cipher key decrypt failed: {e}"))?,
            );
            let legacy = BitwardenLegacyKeyBytes::from(raw.to_vec());
            SymmetricCryptoKey::try_from(&legacy)
                .map_err(|e| anyhow!("cipher key is not a symmetric key: {e}"))
        }
        _ => Ok(base.clone()),
    }
}

pub fn wrap_symmetric_key(
    key: &SymmetricCryptoKey,
    wrapping: &SymmetricCryptoKey,
) -> Result<String> {
    let bytes = BitwardenLegacyKeyBytes::from(key.to_encoded().as_ref().to_vec());
    (&bytes)
        .encrypt_with_key(wrapping)
        .map(|enc| enc.to_string())
        .map_err(|e| anyhow!("wrapping a symmetric key failed: {e}"))
}

pub fn unwrap_symmetric_key(
    enc: &str,
    wrapping: &SymmetricCryptoKey,
) -> Result<SymmetricCryptoKey> {
    let parsed: EncString =
        enc.parse().map_err(|e| anyhow!("wrapped key EncString parse failed: {e}"))?;
    let raw = Zeroizing::<Vec<u8>>::new(
        parsed
            .decrypt_with_key(wrapping)
            .map_err(|e| anyhow!("wrapped key decrypt failed: {e}"))?,
    );
    let legacy = BitwardenLegacyKeyBytes::from(raw.to_vec());
    SymmetricCryptoKey::try_from(&legacy)
        .map_err(|e| anyhow!("unwrapped key is not a symmetric key: {e}"))
}

pub fn encrypt_str(value: &str, key: &SymmetricCryptoKey) -> Result<String> {
    value
        .encrypt_with_key(key)
        .map(|enc| enc.to_string())
        .map_err(|e| anyhow!("EncString encrypt failed: {e}"))
}

/// One computed TOTP code and the window it belongs to.
pub struct Totp {
    pub code: String,
    pub period: u64,
    pub remaining: u64,
}

/// Compute a TOTP code from the stored `login.totp` value: an otpauth URI, or
/// a bare base32 secret (Bitwarden accepts both). The seed never leaves the
/// process.
pub fn totp_from_uri(uri: &str, now: std::time::SystemTime) -> Result<Totp> {
    let (secret, digits, period, algorithm) = if uri.trim_start().starts_with("otpauth://") {
        let parsed = url::Url::parse(uri.trim()).context("parsing the otpauth URI")?;
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        let secret = pairs.get("secret").cloned().context("otpauth URI has no secret")?;
        let digits = pairs.get("digits").and_then(|v| v.parse().ok()).unwrap_or(6u32);
        let period = pairs.get("period").and_then(|v| v.parse().ok()).unwrap_or(30u64);
        let algorithm =
            pairs.get("algorithm").map_or_else(|| "SHA1".to_owned(), |v| v.to_uppercase());
        (secret, digits, period, algorithm)
    } else {
        (uri.trim().to_owned(), 6u32, 30u64, "SHA1".to_owned())
    };
    // The RFCs and every authenticator app issue 6 to 8 digits; a tenth digit
    // overflows the u32 the truncation computes in.
    if !(6..=8).contains(&digits) {
        bail!("TOTP digits out of range: {digits}");
    }
    if period == 0 {
        bail!("TOTP period is zero");
    }
    let key = decode_base32(&secret)?;
    let unix =
        now.duration_since(std::time::UNIX_EPOCH).context("system clock is before 1970")?.as_secs();
    let counter = unix / period;
    let remaining = period - (unix % period);

    let digest = match algorithm.as_str() {
        "SHA1" => {
            let mut mac =
                Hmac::<sha1::Sha1>::new_from_slice(&key).map_err(|e| anyhow!("HMAC init: {e}"))?;
            mac.update(&counter.to_be_bytes());
            mac.finalize().into_bytes().to_vec()
        }
        "SHA256" => {
            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key)
                .map_err(|e| anyhow!("HMAC init: {e}"))?;
            mac.update(&counter.to_be_bytes());
            mac.finalize().into_bytes().to_vec()
        }
        "SHA512" => {
            let mut mac = Hmac::<sha2::Sha512>::new_from_slice(&key)
                .map_err(|e| anyhow!("HMAC init: {e}"))?;
            mac.update(&counter.to_be_bytes());
            mac.finalize().into_bytes().to_vec()
        }
        other => bail!("unsupported TOTP algorithm: {other}"),
    };

    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = (u32::from(digest[offset] & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    let code = binary % 10u32.pow(digits);
    Ok(Totp { code: format!("{code:0width$}", width = digits as usize), period, remaining })
}

fn decode_base32(secret: &str) -> Result<Vec<u8>> {
    let cleaned: String = secret
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .collect::<String>()
        .to_uppercase();
    data_encoding::BASE32_NOPAD.decode(cleaned.as_bytes()).context("TOTP secret is not base32")
}

/// Encrypt an attachment into the uploaded wire form: `0x02 | IV | MAC |
/// ciphertext`, identical to `EncString::to_buffer`.
pub fn encrypt_attachment_blob(data: &[u8], key: &SymmetricCryptoKey) -> Result<Vec<u8>> {
    let typed = OctetStreamBytes::from(data.to_vec());
    let encrypted =
        (&typed).encrypt_with_key(key).map_err(|e| anyhow!("attachment encrypt failed: {e}"))?;
    wire_from_enc_string(&encrypted)
}

fn wire_from_enc_string(encrypted: &EncString) -> Result<Vec<u8>> {
    match encrypted {
        EncString::Aes256Cbc_HmacSha256_B64 { iv, mac, data } => {
            let mut out = Vec::with_capacity(49 + data.len());
            out.push(0x02);
            out.extend_from_slice(iv);
            out.extend_from_slice(mac);
            out.extend_from_slice(data);
            Ok(out)
        }
        _ => bail!("attachment encryption did not produce an AES-256-CBC-HMAC string"),
    }
}

/// Decrypt a downloaded attachment blob.
pub fn decrypt_attachment_blob(wire: &[u8], key: &SymmetricCryptoKey) -> Result<Vec<u8>> {
    if wire.first() != Some(&0x02) {
        bail!("unknown attachment format header (expected 0x02)");
    }
    if wire.len() < 49 + 16 {
        bail!("attachment blob is too short to be valid");
    }
    let iv: [u8; 16] = wire[1..17].try_into().map_err(|_| anyhow!("bad IV length"))?;
    let mac: [u8; 32] = wire[17..49].try_into().map_err(|_| anyhow!("bad MAC length"))?;
    let data = wire[49..].to_vec();
    let encrypted = EncString::Aes256Cbc_HmacSha256_B64 { iv, mac, data };
    encrypted.decrypt_with_key(key).map_err(|e| anyhow!("attachment decrypt failed: {e}"))
}

pub fn sha256_hex(data: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(data))
}

/// A fresh symmetric key for one attachment, in the algorithm the live
/// server stores its blobs with.
pub fn make_symmetric_key() -> SymmetricCryptoKey {
    SymmetricCryptoKey::make(bitwarden_crypto::SymmetricKeyAlgorithm::Aes256CbcHmac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitwarden_crypto::SymmetricKeyAlgorithm;
    use rsa::pkcs8::EncodePrivateKey;

    /// The vector is bitwarden-crypto 4.0.0's own PBKDF2 test: password
    /// "asdfasdf", salt "test@bitwarden.com", 100k iterations.
    #[test]
    fn master_password_hash_matches_the_reference_vector() {
        let pre =
            Prelogin { kdf: 0, kdf_iterations: 100_000, kdf_memory: None, kdf_parallelism: None };
        let master_key = derive_master_key("asdfasdf", "test@bitwarden.com", &pre).unwrap();
        assert_eq!(
            master_password_hash(&master_key, "asdfasdf"),
            "wmyadRMyBZOH7P/a/ucTCbSghKgdzDpPqUnu/DAVtSw="
        );
    }

    #[test]
    fn email_is_trimmed_and_lowercased_as_the_salt() {
        let pre =
            Prelogin { kdf: 0, kdf_iterations: 100_000, kdf_memory: None, kdf_parallelism: None };
        let a = derive_master_key("asdfasdf", "test@bitwarden.com", &pre).unwrap();
        let b = derive_master_key("asdfasdf", "  TEST@bitwarden.com ", &pre).unwrap();
        assert_eq!(master_password_hash(&a, "asdfasdf"), master_password_hash(&b, "asdfasdf"));
    }

    #[test]
    fn unknown_kdf_is_a_named_error() {
        let pre = Prelogin { kdf: 7, kdf_iterations: 1, kdf_memory: None, kdf_parallelism: None };
        assert!(derive_master_key("x", "y", &pre).is_err());
    }

    /// The live account's private key decrypts to raw PKCS#8 DER, not PEM.
    #[test]
    fn unlock_parses_a_private_key_stored_as_raw_der() {
        let pre =
            Prelogin { kdf: 0, kdf_iterations: 100_000, kdf_memory: None, kdf_parallelism: None };
        let master_key = derive_master_key("asdfasdf", "test@bitwarden.com", &pre).unwrap();
        let (user_key, wrapped_user_key) = master_key.make_user_key().unwrap();
        let user_key = user_key.0;

        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 1024).unwrap();
        let der = rsa_key.to_pkcs8_der().unwrap();
        let der_bytes = BitwardenLegacyKeyBytes::from(der.as_bytes().to_vec());
        let wrapped_private_key = (&der_bytes).encrypt_with_key(&user_key).unwrap().to_string();

        let token = TokenResponse {
            access_token: "token".to_owned(),
            refresh_token: None,
            key: Some(wrapped_user_key.to_string()),
            private_key: Some(wrapped_private_key),
        };
        let unlocked = unlock(&token, &master_key, &Profile::default()).unwrap();
        assert!(unlocked.private_key.is_some());
    }

    /// The release path: a user key a client holds unlocks the same keys the
    /// password does, with the same fingerprint. This is what lets a start
    /// stop crossing the master password.
    #[test]
    fn unlock_with_a_released_user_key_matches_the_password_unlock() {
        let pre =
            Prelogin { kdf: 0, kdf_iterations: 100_000, kdf_memory: None, kdf_parallelism: None };
        let master_key = derive_master_key("asdfasdf", "test@bitwarden.com", &pre).unwrap();
        let (user_key, wrapped_user_key) = master_key.make_user_key().unwrap();
        let user_key = user_key.0;

        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 1024).unwrap();
        let der = rsa_key.to_pkcs8_der().unwrap();
        let der_bytes = BitwardenLegacyKeyBytes::from(der.as_bytes().to_vec());
        let wrapped_private_key = (&der_bytes).encrypt_with_key(&user_key).unwrap().to_string();
        let token = TokenResponse {
            access_token: "token".to_owned(),
            refresh_token: None,
            key: Some(wrapped_user_key.to_string()),
            private_key: Some(wrapped_private_key),
        };

        let from_password =
            unlock(&token, &master_key, &Profile::default()).expect("the password unlock");
        let released = user_key.to_base64().to_string();
        let from_release = unlock_with_user_key(&token, &released, &Profile::default())
            .expect("the release unlock");

        assert_eq!(from_release.user_key, from_password.user_key, "the same user key");
        assert_eq!(
            from_release.user_key_fingerprint, from_password.user_key_fingerprint,
            "the same wrapped-key fingerprint, which the rotation check reads",
        );
        assert!(from_release.private_key.is_some(), "the private key unwraps from the release");
        assert_eq!(from_release.failed_orgs, from_password.failed_orgs);
    }

    /// A released user key that is not a symmetric key is refused rather than
    /// guessed at.
    #[test]
    fn a_released_user_key_that_is_not_a_key_is_refused() {
        let token = TokenResponse {
            access_token: "t".to_owned(),
            refresh_token: None,
            key: Some("2.x|y|z".to_owned()),
            private_key: None,
        };
        assert!(unlock_with_user_key(&token, "not a key", &Profile::default()).is_err());
    }

    /// **A well-formed key that is not the account's is refused.** With the
    /// account's private key present, a valid user key from elsewhere cannot
    /// decrypt it, so the release path does not accept a key on shape alone.
    #[test]
    fn a_mismatched_but_valid_user_key_is_refused() {
        let pre =
            Prelogin { kdf: 0, kdf_iterations: 100_000, kdf_memory: None, kdf_parallelism: None };
        let account = derive_master_key("asdfasdf", "test@bitwarden.com", &pre).unwrap();
        let (user_key, wrapped_user_key) = account.make_user_key().unwrap();
        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 1024).unwrap();
        let der = rsa_key.to_pkcs8_der().unwrap();
        let der_bytes = BitwardenLegacyKeyBytes::from(der.as_bytes().to_vec());
        let wrapped_private_key = (&der_bytes).encrypt_with_key(&user_key.0).unwrap().to_string();
        let token = TokenResponse {
            access_token: "token".to_owned(),
            refresh_token: None,
            key: Some(wrapped_user_key.to_string()),
            private_key: Some(wrapped_private_key),
        };

        let elsewhere = derive_master_key("another password", "other@example.com", &pre).unwrap();
        let (wrong, _) = elsewhere.make_user_key().unwrap();
        let refused =
            unlock_with_user_key(&token, &wrong.0.to_base64().to_string(), &Profile::default());

        assert!(refused.is_err(), "a valid key from another account is refused");
    }

    /// An empty `PrivateKey` on the token is a client's unset value, not a
    /// wrapped key; it must not hide the profile's real one.
    #[test]
    fn an_empty_token_private_key_falls_back_to_the_profile_key() {
        let pre =
            Prelogin { kdf: 0, kdf_iterations: 100_000, kdf_memory: None, kdf_parallelism: None };
        let master_key = derive_master_key("asdfasdf", "test@bitwarden.com", &pre).unwrap();
        let (user_key, wrapped_user_key) = master_key.make_user_key().unwrap();
        let user_key = user_key.0;

        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 1024).unwrap();
        let der = rsa_key.to_pkcs8_der().unwrap();
        let der_bytes = BitwardenLegacyKeyBytes::from(der.as_bytes().to_vec());
        let wrapped_private_key = (&der_bytes).encrypt_with_key(&user_key).unwrap().to_string();

        let token = TokenResponse {
            access_token: "token".to_owned(),
            refresh_token: None,
            key: Some(wrapped_user_key.to_string()),
            private_key: Some(String::new()),
        };
        let profile = Profile { private_key: Some(wrapped_private_key), ..Profile::default() };

        let unlocked = unlock(&token, &master_key, &profile).expect("the profile key is used");

        assert!(unlocked.private_key.is_some(), "the profile private key was unwrapped");
    }

    /// The org-key unwrap is the one hand-rolled crypto path; round-trip it the
    /// way Vaultwarden stores `profile.organizations[].key` (EncString type 4).
    #[test]
    fn org_key_unwrap_round_trips_a_type_4_wrap() {
        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 2048).unwrap();
        let public_key = rsa_key.to_public_key();
        let key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let encoded = key.to_encoded();
        let ciphertext = public_key
            .encrypt(&mut bitwarden_random::rng(), rsa::Oaep::<sha1::Sha1>::new(), encoded.as_ref())
            .unwrap();

        let data = base64::engine::general_purpose::STANDARD.encode(ciphertext);
        let unwrapped = unwrap_org_key(&format!("4.{data}"), &rsa_key).unwrap();

        assert_eq!(unwrapped, key);
    }

    #[test]
    fn org_key_of_a_non_rsa_type_is_refused() {
        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 1024).unwrap();
        let err = unwrap_org_key("2.abc|def|ghi", &rsa_key).unwrap_err().to_string();
        assert!(err.contains("type 2"), "unexpected error: {err}");
    }

    #[test]
    fn totp_matches_the_rfc_6238_sha1_vector() {
        // RFC 6238 Appendix B, SHA1: secret "12345678901234567890", T=59 ->
        // 94287082 (8 digits).
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(59);
        let totp = totp_from_uri(
            "otpauth://totp/spike?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=8&period=30",
            at,
        )
        .unwrap();
        assert_eq!(totp.code, "94287082");
        assert_eq!(totp.period, 30);
        assert_eq!(totp.remaining, 1);
    }

    #[test]
    fn totp_rejects_a_non_base32_secret() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(59);
        assert!(totp_from_uri("otpauth://totp/x?secret=not!base32", at).is_err());
    }

    /// A tenth digit overflows the u32 the truncation computes in; the range
    /// is the 6 to 8 every issuer actually uses.
    #[test]
    fn totp_rejects_a_digit_count_outside_six_to_eight() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(59);
        let uri = |digits: u32| {
            format!("otpauth://totp/x?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits={digits}")
        };
        let overflow = totp_from_uri(&uri(10), at).err().expect("ten digits must not overflow");
        assert!(
            overflow.to_string().contains("digits out of range"),
            "the refusal names the bound: {overflow}"
        );
        assert!(totp_from_uri(&uri(5), at).is_err());
        assert!(totp_from_uri(&uri(6), at).is_ok());
        assert!(totp_from_uri(&uri(8), at).is_ok());
    }

    /// Argon2id without its memory and parallelism is not a parameter set;
    /// defaulting would derive a key the account does not hold.
    #[test]
    fn argon2id_requires_its_parameters() {
        let missing =
            Prelogin { kdf: 1, kdf_iterations: 3, kdf_memory: None, kdf_parallelism: Some(4) };
        assert!(derive_master_key("x", "y", &missing).is_err());
        let missing_parallelism =
            Prelogin { kdf: 1, kdf_iterations: 3, kdf_memory: Some(64), kdf_parallelism: None };
        assert!(derive_master_key("x", "y", &missing_parallelism).is_err());
        let complete =
            Prelogin { kdf: 1, kdf_iterations: 3, kdf_memory: Some(64), kdf_parallelism: Some(4) };
        assert!(derive_master_key("x", "y", &complete).is_ok());
    }

    /// An empty `Key` field is a client's unset value, not a wrapped key, and
    /// must not shadow the profile's real one.
    #[test]
    fn an_empty_token_key_falls_back_to_the_profile_key() {
        let pre =
            Prelogin { kdf: 0, kdf_iterations: 100_000, kdf_memory: None, kdf_parallelism: None };
        let master_key = derive_master_key("asdfasdf", "test@bitwarden.com", &pre).unwrap();
        let (user_key, wrapped_user_key) = master_key.make_user_key().unwrap();
        drop(user_key);

        let token = TokenResponse {
            access_token: "token".to_owned(),
            refresh_token: None,
            key: Some(String::new()),
            private_key: None,
        };
        let profile = Profile { key: Some(wrapped_user_key.to_string()), ..Profile::default() };

        let unlocked = unlock(&token, &master_key, &profile).expect("the profile key is used");

        assert!(unlocked.failed_orgs.is_empty());
    }

    /// An organization cipher whose key this unlock did not get is refused by
    /// name rather than encrypted or decrypted with the user key.
    #[test]
    fn an_organization_cipher_without_its_key_is_refused() {
        let unlocked = Unlocked {
            user_key: SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac),
            private_key: None,
            org_keys: HashMap::new(),
            failed_orgs: vec!["org-1".to_owned()],
            user_key_fingerprint: String::new(),
        };
        let org_cipher = serde_json::json!({ "organizationId": "org-1", "name": "x" });

        let error = unlocked.decrypt_item_str(&org_cipher, "2.aGk=|ZGF0YQ==|bWFj").unwrap_err();

        assert!(matches!(&error, VaultError::MissingKey(org_id) if org_id == "org-1"), "{error}");

        let personal = serde_json::json!({ "name": "x" });
        let enc = encrypt_str("hello", &unlocked.user_key).unwrap();
        assert_eq!(unlocked.decrypt_item_str(&personal, &enc).unwrap(), "hello");
    }

    /// A failed org unwrap is recorded by id, so the session can report it and
    /// its ciphers fail closed.
    #[test]
    fn a_failed_org_key_is_recorded_by_id() {
        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 1024).unwrap();
        let profile = Profile {
            organizations: vec![crate::vault::models::Organization {
                id: "org-1".to_owned(),
                key: Some("2.aGk=|ZGF0YQ==|bWFj".to_owned()),
                name: None,
            }],
            ..Profile::default()
        };

        let (keys, failed) = org_keys(Some(&rsa_key), &profile);

        assert!(keys.is_empty());
        assert_eq!(failed, vec!["org-1"]);
    }

    #[test]
    fn attachment_blob_round_trips_and_rejects_tampering() {
        let key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let data = b"vault attachment contents, round trip";
        let wire = encrypt_attachment_blob(data, &key).unwrap();
        assert_eq!(wire[0], 0x02);
        assert_eq!(decrypt_attachment_blob(&wire, &key).unwrap(), data);

        let mut tampered = wire.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(decrypt_attachment_blob(&tampered, &key).is_err());
    }

    /// The live failure shape: a 47-byte plaintext becomes 48 bytes of CBC
    /// ciphertext, so the stored blob is exactly 97 bytes with the 0x02 header.
    #[test]
    fn attachment_wire_layout_is_discriminator_iv_mac_ciphertext() {
        let key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let data = b"layout";
        let typed = OctetStreamBytes::from(data.to_vec());
        let encrypted = (&typed).encrypt_with_key(&key).unwrap();
        let EncString::Aes256Cbc_HmacSha256_B64 { iv, mac, data: ciphertext } = &encrypted else {
            panic!("expected an AES-256-CBC-HMAC EncString");
        };
        let mut expected = vec![0x02];
        expected.extend_from_slice(iv);
        expected.extend_from_slice(mac);
        expected.extend_from_slice(ciphertext);

        assert_eq!(wire_from_enc_string(&encrypted).unwrap(), expected);
        assert_eq!(&expected[1..17], iv.as_slice());
        assert_eq!(&expected[17..49], mac.as_slice());

        let live_shape = encrypt_attachment_blob(&[b'x'; 47], &key).unwrap();
        assert_eq!(live_shape.len(), 97);
        assert_eq!(live_shape[0], 0x02);
        assert_eq!(decrypt_attachment_blob(&live_shape, &key).unwrap(), vec![b'x'; 47]);
    }

    #[test]
    fn attachment_key_unwraps_when_aes_wrapped_as_type_2() {
        let item_key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let attachment_key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let wrapped = wrap_symmetric_key(&attachment_key, &item_key).unwrap();
        let unwrapped = unwrap_attachment_key(&wrapped, &item_key, None).unwrap();
        assert_eq!(unwrapped, attachment_key);
    }

    /// The real RSA layout: `4.<ciphertext>`, one part, no IV. A parser that
    /// expected an IV at index 0 failed every org and attachment unwrap.
    #[test]
    fn attachment_key_unwraps_when_rsa_wrapped_as_type_4() {
        use base64::Engine;
        let item_key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let attachment_key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 2048).unwrap();
        let public = rsa_key.to_public_key();
        let encoded = attachment_key.to_encoded();
        let ciphertext = public
            .encrypt(&mut bitwarden_random::rng(), rsa::Oaep::<sha1::Sha1>::new(), encoded.as_ref())
            .unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(ciphertext);
        let wrapped = format!("4.{data}");

        let unwrapped = unwrap_attachment_key(&wrapped, &item_key, Some(&rsa_key)).unwrap();
        assert_eq!(unwrapped, attachment_key);
    }

    /// The type-6 layout is `6.<ciphertext>|<mac>`, and the MAC covers the
    /// ciphertext only.
    #[test]
    fn attachment_key_unwraps_when_rsa_wrapped_as_type_6_and_rejects_a_bad_mac() {
        use base64::Engine;
        use hmac::{Hmac, KeyInit, Mac};

        let item_key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let attachment_key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);
        let rsa_key = rsa::RsaPrivateKey::new(&mut bitwarden_random::rng(), 2048).unwrap();
        let public = rsa_key.to_public_key();
        let encoded = attachment_key.to_encoded();
        let ciphertext = public
            .encrypt(&mut bitwarden_random::rng(), rsa::Oaep::<sha1::Sha1>::new(), encoded.as_ref())
            .unwrap();
        let composite = item_key.to_encoded();
        let mut hmac = Hmac::<sha2::Sha256>::new_from_slice(&composite.as_ref()[32..64]).unwrap();
        hmac.update(&ciphertext);
        let mac = hmac.finalize().into_bytes();
        let engine = base64::engine::general_purpose::STANDARD;
        let wrapped = format!("6.{}|{}", engine.encode(&ciphertext), engine.encode(mac));

        let unwrapped = unwrap_attachment_key(&wrapped, &item_key, Some(&rsa_key)).unwrap();
        assert_eq!(unwrapped, attachment_key);

        let mut bad_mac = mac.to_vec();
        bad_mac[0] ^= 0xff;
        let bad = format!("6.{}|{}", engine.encode(&ciphertext), engine.encode(bad_mac));
        let error = unwrap_attachment_key(&bad, &item_key, Some(&rsa_key))
            .expect_err("a tampered MAC is refused");
        assert!(
            error.to_string().contains("MAC did not verify"),
            "the refusal names its own reason: {error}"
        );
    }

    /// An RSA-wrapped attachment key with no private key is not a decrypt
    /// failure: the account has none, and it is named as such.
    #[test]
    fn an_rsa_wrapped_attachment_key_without_a_private_key_is_named() {
        let item_key = SymmetricCryptoKey::make(SymmetricKeyAlgorithm::Aes256CbcHmac);

        let error = unwrap_attachment_key("4.aGk=", &item_key, None).unwrap_err();

        assert!(matches!(error, VaultError::MissingPrivateKey), "{error}");
    }

    #[test]
    fn enc_string_type_reads_the_header_byte_only() {
        assert_eq!(enc_string_type("2.aGk=|ZGF0YQ==|bWFj"), Some(2));
        assert_eq!(enc_string_type("4.aGk=|ZGF0YQ=="), Some(4));
        assert_eq!(enc_string_type("not-an-encstring"), None);
    }

    #[test]
    fn sha256_hex_matches_the_known_digest_of_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
