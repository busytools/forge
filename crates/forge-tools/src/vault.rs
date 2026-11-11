//! The vault family: the Vaultwarden connector, its process-wide session,
//! the thirteen held tools, and the parked calls they raise.
//!
//! The module owns the wire shapes ([`models`]), the encryption primitives
//! ([`crypto`]), the HTTP client ([`client`]), the session's tokens and
//! unlocked account ([`session`]), the description and dispatch of every
//! tool ([`tools`]), what a held call is and what answering it means
//! ([`facade`]), and the registry those calls park in ([`ask`]).
//!
//! # The host seam
//!
//! The workspace holds the machine-wide session and the ask registry; this
//! crate owns their types and policy. What the family needs from its host
//! arrives through [`VaultHost`] (the live session, the caller's working
//! directory) and [`VaultEmit`] (the updates a parked call and a lock
//! change produce); what it reports back arrives through
//! [`forge_primitives::AskSink`] (the ask's number and its settlement),
//! and what a view reads arrives through [`forge_primitives::AskSource`]
//! (`VaultAsks` reports the calls it holds). Nothing here names a
//! workspace type.

pub mod ask;
pub mod client;
pub mod crypto;
pub mod facade;
pub mod models;
pub mod session;
pub mod tools;

pub use ask::VaultAsks;
pub use client::VaultClient;
pub use crypto::Unlocked;
#[cfg(any(test, feature = "testing"))]
pub use facade::MockVaultFacade;
pub use facade::{ProdVaultFacade, VaultFacade, VaultToolError};
pub use models::{
    AttachmentMeta, Folder, Organization, Prelogin, Profile, SyncData, TokenResponse, TwoFactorCode,
};
pub use session::{VaultConfig, VaultSession, VaultState};
pub use tools::VaultTools;

use std::path::PathBuf;

use forge_primitives::SessionSlot;
use forge_primitives::vault::{VaultAskEnding, VaultDraft};
use thiserror::Error;
use uuid::Uuid;

/// One update the vault family produces, in the family's own vocabulary.
///
/// The host maps each onto its wire frame. The variants are the ones the
/// family emits today: a parked call, its settlement, the shared session's
/// lock state and a rotated refresh token.
#[derive(Clone)]
pub enum VaultUpdate {
    /// A call is held; the draft is the card a view draws. An emit that
    /// returns `false` for this variant means no attached client could
    /// answer it, which is what makes the caller fail closed.
    AskPending { key: SessionSlot, draft: VaultDraft },
    /// A held call ended, with the ending every client draws.
    AskResolved { key: SessionSlot, id: Uuid, ending: VaultAskEnding },
    /// The shared session locked or unlocked, which every vault card reads.
    LockChanged { locked: bool },
    /// The refresh token rotated and a client holding an enrolment must
    /// rewrite its blob from this. The token is a secret: the update rides
    /// the live socket and no store keeps it.
    Rotated { refresh_token: String },
}

/// Hand-written because a rotated refresh token is a secret and must never
/// reach a log line through a `{:?}`.
impl std::fmt::Debug for VaultUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AskPending { key, draft } => f
                .debug_struct("VaultUpdate::AskPending")
                .field("key", key)
                .field("draft", draft)
                .finish(),
            Self::AskResolved { key, id, ending } => f
                .debug_struct("VaultUpdate::AskResolved")
                .field("key", key)
                .field("id", id)
                .field("ending", ending)
                .finish(),
            Self::LockChanged { locked } => {
                f.debug_struct("VaultUpdate::LockChanged").field("locked", locked).finish()
            }
            Self::Rotated { .. } => {
                f.write_str("VaultUpdate::Rotated { refresh_token: \"[redacted]\" }")
            }
        }
    }
}

/// Where the family emits its updates. The embedding host implements this;
/// the family never reaches past it.
pub trait VaultEmit: Send + Sync {
    /// Emit one update. `false` means no subscriber would take it - for a
    /// parked call that no client can answer, which ends the call rather
    /// than holding it for ever.
    fn emit(&self, update: VaultUpdate) -> bool;
}

/// What the vault family needs from the host it is registered on.
pub trait VaultHost: Send + Sync {
    /// The live machine-wide session, or `None` when `[vault]` is not
    /// configured. The session is swapped on a `[vault]` reload, so a
    /// caller re-reads it rather than holding one across a held call.
    fn session(&self) -> Option<std::sync::Arc<VaultSession>>;

    /// The calling seat's working directory, which an attachment path must
    /// resolve inside.
    fn cwd(&self, seat: &SessionSlot) -> Option<PathBuf>;
}

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
    #[error("the account has no private key, so its RSA-wrapped items cannot be opened")]
    MissingPrivateKey,
    #[error("the account's keys changed while unlocked; unlock again")]
    KeyRotated,
    #[error("the account's user key could not be verified; unlock again")]
    KeyUnverifiable,
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
