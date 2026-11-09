//! The phone's gate: the Kotlin `BiometricPlugin` owns the Android Keystore
//! key, the stored ciphertext and the prompt; this is the typed handle the
//! gate calls through.
//!
//! The Kotlin half is one alias and one record per purpose, so this file never
//! learns what a purpose means either - it passes the string through.

use std::sync::OnceLock;

use serde::Deserialize;
use tauri::Wry;
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};

use super::{Availability, Gate, Release};

/// The registered bridge, set once by [`init`] before any page exists.
static BRIDGE: OnceLock<PluginHandle<Wry>> = OnceLock::new();

/// Register the Kotlin plugin. Android is always Wry, so the plugin is pinned
/// to it rather than generic.
pub fn init() -> TauriPlugin<Wry> {
    Builder::new("biometric")
        .setup(|_app, api| {
            let handle = api.register_android_plugin("dev.vedhavyas.forge", "BiometricPlugin")?;
            let _ = BRIDGE.set(handle);
            Ok(())
        })
        .build()
}

/// The gate this build ships on the phone.
#[derive(Clone, Copy)]
pub struct AndroidGate;

#[derive(Deserialize)]
struct EnrolledAnswer {
    enrolled: bool,
}

#[derive(serde::Serialize)]
struct PurposeArgs<'a> {
    purpose: &'a str,
}

#[derive(serde::Serialize)]
struct StoreArgs<'a> {
    purpose: &'a str,
    blob: String,
}

impl AndroidGate {
    fn bridge(&self) -> Result<&'static PluginHandle<Wry>, String> {
        BRIDGE.get().ok_or_else(|| "the phone's biometric bridge is not registered".to_owned())
    }
}

impl Gate for AndroidGate {
    async fn availability(&self) -> Availability {
        let answered = match self.bridge() {
            Ok(handle) => handle.run_mobile_plugin_async::<Availability>("availability", ()).await,
            Err(reason) => {
                return Availability { available: false, biometric: false, reason: Some(reason) };
            }
        };
        match answered {
            Ok(answer) => answer,
            Err(error) => {
                Availability { available: false, biometric: false, reason: Some(error.to_string()) }
            }
        }
    }

    async fn enrolled(&self, purpose: &str) -> bool {
        let Ok(handle) = self.bridge() else { return false };
        match handle
            .run_mobile_plugin_async::<EnrolledAnswer>("enrolled", PurposeArgs { purpose })
            .await
        {
            Ok(answer) => answer.enrolled,
            // A bridge that cannot answer is a device this gate cannot be
            // offered on; the caller draws the password.
            Err(_) => false,
        }
    }

    async fn store(&self, purpose: &str, blob: &[u8]) -> Result<(), String> {
        use base64::Engine as _;
        let blob = base64::engine::general_purpose::STANDARD.encode(blob);
        let handle = self.bridge()?;
        handle
            .run_mobile_plugin_async::<serde_json::Value>("store", StoreArgs { purpose, blob })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn release(&self, purpose: &str) -> Release {
        let handle = match self.bridge() {
            Ok(handle) => handle,
            Err(reason) => return Release::Refused { reason },
        };
        match handle.run_mobile_plugin_async::<Release>("release", PurposeArgs { purpose }).await {
            Ok(release) => release,
            Err(error) => Release::Refused { reason: error.to_string() },
        }
    }

    async fn remove(&self, purpose: &str) -> Result<(), String> {
        let handle = self.bridge()?;
        handle
            .run_mobile_plugin_async::<serde_json::Value>("remove", PurposeArgs { purpose })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}
