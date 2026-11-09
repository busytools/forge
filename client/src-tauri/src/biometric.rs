//! The biometric proxy's client half: one gate a consumer calls with a
//! purpose, and the platform implementation behind it.
//!
//! **A consumer stores a purpose's wrapped blob, releases it through the
//! platform authenticator, or drops it.** The gate never learns what a purpose
//! means, so adding a consumer is a new purpose and its own blob shape - not a
//! new gate, a new authenticator call or a new command. `macos` and `android`
//! are the implementations; the web page's WebAuthn path sits beside them.
//!
//! The page reaches this through the commands at the bottom of the file. The
//! gate is async because the phone's Keystore and prompt answer through
//! tauri's plugin bridge; the desktop's Keychain call blocks on the system
//! dialog, so its implementation moves that work to a blocking thread.

#![allow(
    clippy::needless_pass_by_value,
    reason = "tauri's IPC macro hands every command argument by value; a reference is not a \
              CommandArg"
)]

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "macos")]
pub mod macos;

use base64::Engine as _;

/// Whether this platform can answer at all, and with which gesture.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Availability {
    /// True when the platform authenticator can answer for this device.
    pub available: bool,
    /// True when the gesture is a biometric one; false when the fallback is
    /// the device's own password (user presence).
    pub biometric: bool,
    /// Why it cannot answer, when it cannot. The card quotes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What one release ended as, as the card draws it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
///
/// Crate-private rather than public: the page reaches the gate through the
/// commands below, and the trait exists so every platform answers the same
/// five questions.
pub(crate) trait Gate: Send + Sync {
    /// Whether this platform can release at all, and with what gesture.
    fn availability(&self) -> impl std::future::Future<Output = Availability> + Send;
    /// Whether a blob is stored for `purpose`. Asked without releasing: a
    /// checkout of an item's attributes or record, never a decrypt, so no
    /// gesture is required to answer it.
    fn enrolled(&self, purpose: &str) -> impl std::future::Future<Output = bool> + Send;
    /// Store `blob` under `purpose`, replacing any blob already there. The
    /// blob is wrapped by the consumer; this half never reads inside it.
    fn store(
        &self,
        purpose: &str,
        blob: &[u8],
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
    /// Ask the platform authenticator for the blob behind `purpose`.
    fn release(&self, purpose: &str) -> impl std::future::Future<Output = Release> + Send;
    /// Drop the blob behind `purpose`.
    fn remove(&self, purpose: &str)
    -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// The gate this build ships.
#[cfg(target_os = "macos")]
pub(crate) type PlatformGate = macos::KeychainGate;
#[cfg(target_os = "android")]
pub(crate) type PlatformGate = android::AndroidGate;
#[cfg(not(any(target_os = "macos", target_os = "android")))]
pub(crate) type PlatformGate = Unsupported;

/// The gate this build ships, built: a unit on the desktop, the phone's
/// registered bridge handle on Android.
pub(crate) fn platform() -> PlatformGate {
    #[cfg(target_os = "macos")]
    {
        macos::KeychainGate
    }
    #[cfg(target_os = "android")]
    {
        android::AndroidGate
    }
    #[cfg(not(any(target_os = "macos", target_os = "android")))]
    {
        Unsupported
    }
}

/// A build with no platform authenticator: every call refuses with the reason,
/// which is what a desktop forge does not ship for yet.
#[cfg(not(any(target_os = "macos", target_os = "android")))]
pub(crate) struct Unsupported;

#[cfg(not(any(target_os = "macos", target_os = "android")))]
impl Gate for Unsupported {
    fn availability(&self) -> impl std::future::Future<Output = Availability> + Send {
        std::future::ready(Availability {
            available: false,
            biometric: false,
            reason: Some("this platform has no biometric gate yet".to_owned()),
        })
    }

    fn enrolled(&self, _purpose: &str) -> impl std::future::Future<Output = bool> + Send {
        std::future::ready(false)
    }

    fn store(
        &self,
        _purpose: &str,
        _blob: &[u8],
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        std::future::ready(Err("this platform has no biometric gate yet".to_owned()))
    }

    fn release(&self, _purpose: &str) -> impl std::future::Future<Output = Release> + Send {
        std::future::ready(Release::Refused {
            reason: "this platform has no biometric gate yet".to_owned(),
        })
    }

    fn remove(
        &self,
        _purpose: &str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        std::future::ready(Err("this platform has no biometric gate yet".to_owned()))
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
pub async fn biometric_availability() -> Availability {
    platform().availability().await
}

/// Whether this device holds a blob for `purpose`.
#[tauri::command]
pub async fn biometric_enrolled(purpose: String) -> bool {
    platform().enrolled(&purpose).await
}

/// Register one purpose's wrapped blob. **Async**, because the store may ask
/// the platform authenticator behind the scene on some platforms, and the
/// window must not wait on it.
#[tauri::command]
pub async fn biometric_store(purpose: String, blob: String) -> Result<(), String> {
    let blob = decode(&blob)?;
    platform().store(&purpose, &blob).await
}

/// Release one purpose's blob, asking the platform authenticator first.
/// **Async**, because the OS prompt blocks this call until the person answers.
#[tauri::command]
pub async fn biometric_release(purpose: String) -> Release {
    platform().release(&purpose).await
}

/// Drop one purpose's blob.
#[tauri::command]
pub async fn biometric_remove(purpose: String) -> Result<(), String> {
    platform().remove(&purpose).await
}
