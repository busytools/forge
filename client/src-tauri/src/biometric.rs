//! The biometric proxy's client half: one gate a consumer calls with a
//! purpose, and the platform implementation behind it.
//!
//! **A consumer stores a purpose's wrapped blob, releases it through the
//! platform authenticator, or drops it.** The gate never learns what a purpose
//! means, so adding a consumer is a new purpose and its own blob shape - not a
//! new gate, a new authenticator call or a new command. `macos` is the first
//! implementation; Android and the web page's WebAuthn path sit beside it.
//!
//! The page reaches this through the commands at the bottom of the file. A
//! release blocks while the OS shows its prompt, so the commands that can
//! prompt run on the async runtime rather than the window's main thread.

#![allow(
    clippy::needless_pass_by_value,
    reason = "tauri's IPC macro hands every command argument by value; a reference is not a \
              CommandArg"
)]

#[cfg(target_os = "macos")]
pub mod macos;

use std::sync::Arc;

use base64::Engine as _;

/// Whether this platform can answer at all, and with which gesture.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Availability {
    /// True when the platform authenticator can answer for this device.
    pub available: bool,
    /// True when the gesture is a biometric one; false when the fallback is
    /// the device's own password (user presence).
    pub biometric: bool,
    /// Why it cannot answer, when it cannot. The card quotes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What one release ended as, as the card draws it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Release {
    /// The platform released the blob, base64 on this boundary.
    Released { blob: String },
    /// The platform did not release it: a declined or failed gesture, a
    /// locked-out authenticator, or a store that would not answer. `reason` is
    /// the platform's own words, and the card offers the password beside it.
    Refused { reason: String },
    /// Nothing is stored for that purpose on this device, so there is nothing
    /// to release and the password is the only path.
    NotEnrolled,
}

/// One platform's authenticator, addressed by purpose.
pub trait Gate: Send + Sync {
    /// Whether this platform can release at all, and with what gesture.
    fn availability(&self) -> Availability;
    /// Whether a blob is stored for `purpose`. Asked without releasing: a
    /// checkout of an item's attributes, never its data, so no gesture is
    /// required to answer it.
    fn enrolled(&self, purpose: &str) -> bool;
    /// Store `blob` under `purpose`, replacing any blob already there. The
    /// blob is wrapped by the consumer; this half never reads inside it.
    fn store(&self, purpose: &str, blob: &[u8]) -> Result<(), String>;
    /// Ask the platform authenticator for the blob behind `purpose`.
    fn release(&self, purpose: &str) -> Release;
    /// Drop the blob behind `purpose`.
    fn remove(&self, purpose: &str) -> Result<(), String>;
}

/// The gate this build ships.
pub fn platform() -> Arc<dyn Gate> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::KeychainGate)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(Unsupported)
    }
}

/// A build with no platform authenticator: every call refuses with the reason,
/// which is what a desktop forge does not ship for yet.
#[cfg(not(target_os = "macos"))]
struct Unsupported;

#[cfg(not(target_os = "macos"))]
impl Gate for Unsupported {
    fn availability(&self) -> Availability {
        Availability {
            available: false,
            biometric: false,
            reason: Some("this platform has no biometric gate yet".to_owned()),
        }
    }

    fn enrolled(&self, _purpose: &str) -> bool {
        false
    }

    fn store(&self, _purpose: &str, _blob: &[u8]) -> Result<(), String> {
        Err("this platform has no biometric gate yet".to_owned())
    }

    fn release(&self, _purpose: &str) -> Release {
        Release::Refused { reason: "this platform has no biometric gate yet".to_owned() }
    }

    fn remove(&self, _purpose: &str) -> Result<(), String> {
        Err("this platform has no biometric gate yet".to_owned())
    }
}

/// The blob's base64, as the page and the socket both carry it, decoded for a
/// gate that wants bytes.
fn decode(blob: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(blob.as_bytes())
        .map_err(|error| format!("the wrapped blob is not base64: {error}"))
}

/// The page's door to the gate: what this device can do, and with what.
#[tauri::command]
pub fn biometric_availability() -> Availability {
    platform().availability()
}

/// Whether this device holds a blob for `purpose`.
#[tauri::command]
pub fn biometric_enrolled(purpose: String) -> bool {
    platform().enrolled(&purpose)
}

/// Register one purpose's wrapped blob. **Async**, because the store may ask
/// the platform authenticator behind the scene on some platforms, and the
/// window must not wait on it.
#[tauri::command(async)]
pub fn biometric_store(purpose: String, blob: String) -> Result<(), String> {
    let blob = decode(&blob)?;
    platform().store(&purpose, &blob)
}

/// Release one purpose's blob, asking the platform authenticator first.
/// **Async**, because the OS prompt blocks this call until the person answers.
#[tauri::command(async)]
pub fn biometric_release(purpose: String) -> Release {
    platform().release(&purpose)
}

/// Drop one purpose's blob.
#[tauri::command(async)]
pub fn biometric_remove(purpose: String) -> Result<(), String> {
    platform().remove(&purpose)
}
