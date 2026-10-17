//! What the phone's platform is doing to this app's microphone, bridged: the
//! Kotlin `MicStatePlugin` reads the facts, and this is the typed handle the
//! shell's `client_mic_state` answers by. Android-only, because no other
//! platform reports anything of the kind - the desktop arm answers `None` and
//! the page draws its line without a cause.

use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{Manager as _, Runtime};

use crate::mic::MicState;

/// The Kotlin handle, one call: the facts, read live rather than mirrored - a
/// state read when the row needs it can never be a state from a take ago.
pub struct Reader<R: Runtime = tauri::Wry>(PluginHandle<R>);

impl Reader {
    /// The platform's answer right now.
    pub async fn state(&self) -> Result<MicState, String> {
        self.0.run_mobile_plugin_async("micState", ()).await.map_err(|err| err.to_string())
    }
}

/// Register the Kotlin mic-state plugin and manage its handle. Android is
/// always Wry, so the plugin is pinned to it rather than generic.
pub fn init() -> TauriPlugin<tauri::Wry> {
    Builder::new("micstate")
        .setup(|app, api| {
            let handle = api.register_android_plugin("dev.vedhavyas.forge", "MicStatePlugin")?;
            app.manage(std::sync::Arc::new(Reader(handle)));
            Ok(())
        })
        .build()
}
