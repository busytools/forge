//! OAuth bearer-credential wire shape.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// OAuth bearer credentials persisted by the `claude` CLI at
/// `<config_dir>/.credentials.json`.
///
/// `<config_dir>` resolves to `$CLAUDE_CONFIG_DIR` when set and
/// non-empty, else `$HOME/.claude`. The file format is
/// `{ "claudeAiOauth": { "accessToken": "...", "expiresAt": <epoch> } }`
/// where `expiresAt` is either a numeric epoch (seconds OR
/// milliseconds) or a numeric string. The loader in
/// `forge_agent::cloud::oauth_credentials` normalises both shapes
/// during deserialisation; serialisation emits a `SystemTime`
/// directly, which is fine for in-memory use but is NOT a stable wire
/// shape - don't round-trip these through anything but the live
/// in-memory reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OauthCredentials {
    /// The bearer token to send as `Authorization: Bearer <token>` to
    /// `api.anthropic.com`.
    pub access_token: String,
    /// Optional absolute expiry. Callers typically check
    /// `expires_at <= SystemTime::now()` before making outbound
    /// requests; `None` means the file did not include an expiry
    /// field.
    pub expires_at: Option<SystemTime>,
}
