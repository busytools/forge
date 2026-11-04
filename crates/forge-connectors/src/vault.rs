//! The Vaultwarden connector: the Bitwarden client API this family talks to.
//!
//! The module owns the wire shapes ([`models`]), the encryption primitives
//! ([`crypto`]) and the HTTP client ([`client`]). It holds no session state:
//! the workspace's `VaultSession` owns the tokens and the unlocked account, and
//! hands this module a token per call. An [`Unlocked`] value - the derived keys
//! and the decrypt/encrypt operations over them - crosses into the workspace
//! that holds it. Everything here is testable offline against a local server.

pub mod client;
pub mod crypto;
pub mod models;

pub use client::VaultClient;
pub use crypto::Unlocked;
pub use models::{
    AttachmentMeta, Folder, Organization, Prelogin, Profile, SyncData, TokenResponse, TwoFactorCode,
};

use thiserror::Error;

/// Everything the connector can fail with, named so the facade can branch on it
/// without parsing strings. A 401 is the refresh trigger; a 404 names a missing
/// item; a 429 carries the server's retry hint.
#[derive(Debug, Error)]
pub enum VaultError {
    /// The failure class only. A signed download URL carries its token in the
    /// query string, so no error may print the URL it was sent to.
    #[error("request failed: {}", http_class(.0))]
    Http(#[from] reqwest::Error),
    #[error("no unlocked key for organization {0}")]
    MissingKey(String),
    #[error("invalid request: {0}")]
    InvalidInput(String),
    #[error("refused attachment URL: {0}")]
    InvalidUrl(String),
    #[error("the attachment is {size} bytes, over the {cap} byte cap")]
    AttachmentTooLarge { size: u64, cap: u64 },
    #[error("vault is locked")]
    Locked,
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("rate limited; retry after {retry_after:?} seconds")]
    RateLimited { retry_after: Option<u64> },
    #[error("API error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("crypto error: {0}")]
    Crypto(#[from] anyhow::Error),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

fn http_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_body() {
        "the response body could not be read"
    } else if error.is_decode() {
        "the response could not be decoded"
    } else if error.is_request() {
        "the request could not be sent"
    } else {
        "the request failed"
    }
}
