//! The Vaultwarden connector: the Bitwarden client API this family talks to.
//!
//! The module owns the wire shapes ([`models`]), the encryption primitives
//! ([`crypto`]) and the HTTP client ([`client`]). It holds no session state and
//! no keys: the workspace's `VaultSession` owns those and hands this module a
//! token per call. Everything here is testable offline against a local server.

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
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
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

impl VaultError {
    /// The HTTP status this error stands for, when it came from the API. The
    /// facade uses this to tell a 401 refresh from a 404 missing item without
    /// matching on the body text.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Unauthorized => Some(401),
            Self::Forbidden(_) => Some(403),
            Self::NotFound(_) => Some(404),
            Self::RateLimited { .. } => Some(429),
            Self::Api { status, .. } => Some(*status),
            Self::Http(_) | Self::Locked | Self::Crypto(_) | Self::InvalidResponse(_) => None,
        }
    }
}
