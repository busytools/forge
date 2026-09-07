//! OAuth bearer-credential wire shape.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// OAuth bearer credentials: the access token a session authenticates
/// with and, when the source records one, its expiry. forge builds
/// these from an account's env token, which carries no locally-known
/// expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OauthCredentials {
    /// The bearer token to send as `Authorization: Bearer <token>` to
    /// `api.anthropic.com`.
    pub access_token: String,
    /// Optional absolute expiry. Callers typically check
    /// `expires_at <= SystemTime::now()` before making outbound
    /// requests; `None` means the source recorded no expiry.
    pub expires_at: Option<SystemTime>,
}
