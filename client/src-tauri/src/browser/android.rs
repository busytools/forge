//! The phone's engine, bridged: the Kotlin `BrowserPlugin` owns the WebView,
//! the CDP relay and the in-app node; this is the typed handle the host
//! drives it by. The desktop's equivalents live in `chromium.rs` (a launched
//! browser) and `driver.rs`'s child spawn.
//!
//! The Kotlin side registers at the plugin's setup, and the host is built
//! there too - the engine handle lives IN the host, so no second managed
//! state has to be fetched from an `AppHandle` the host methods do not have.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{Manager as _, Runtime};

/// What the Kotlin engine reported when the driver started: the relay the
/// in-app node speaks CDP through, the client UI's origin (the driver's tab
/// pin must steer off it), and whether THIS call launched node - a cold
/// boot takes seconds, while an already-running node redials every second,
/// so the shell sizes its accept window by this.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Relay {
    #[serde(rename = "relayPort")]
    pub relay_port: u16,
    #[serde(rename = "uiOrigin")]
    pub ui_origin: String,
    #[serde(rename = "nodeStarted")]
    pub node_started: bool,
    /// The engine's real viewport, which the driver's context is seeded with
    /// right after the pin (see `Driver::start_inapp`).
    #[serde(rename = "viewportWidth", default)]
    pub viewport_width: u32,
    #[serde(rename = "viewportHeight", default)]
    pub viewport_height: u32,
    /// Bumped by a claimed renderer death. The host's driver identity is
    /// this generation, so a death makes the held driver stale and the next
    /// call rebuilds against the fresh WebView (see `BrowserHost::call`'s
    /// phone arm).
    #[serde(rename = "engineGeneration", default)]
    pub engine_generation: u64,
}

#[derive(Serialize)]
struct StartArgs {
    #[serde(rename = "socketPath")]
    socket_path: String,
    #[serde(rename = "outputDir")]
    output_dir: String,
}

#[derive(Deserialize)]
struct Windowed {
    windowed: bool,
}

/// The Kotlin handle, and the last engine generation it reported (a claimed
/// renderer death bumps it; see `Relay::engine_generation`).
pub struct Engine<R: Runtime = tauri::Wry>(
    PluginHandle<R>,
    std::sync::atomic::AtomicU64,
);

impl Engine {
    /// The engine's generation, as the last start reported it.
    pub fn generation(&self) -> u64 {
        self.1.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Bring the engine up (WebView, relay, unpacked tree) and start the
    /// in-app node, which dials `socket`. Answers the relay facts.
    pub async fn start_driver(&self, socket: &Path, output: &Path) -> Result<Relay, String> {
        let args = StartArgs {
            socket_path: socket.to_string_lossy().into_owned(),
            output_dir: output.to_string_lossy().into_owned(),
        };
        let relay: Relay = self
            .0
            .run_mobile_plugin_async("startDriver", args)
            .await
            .map_err(|err| err.to_string())?;
        self.1.store(relay.engine_generation, std::sync::atomic::Ordering::Release);
        Ok(relay)
    }

    /// Bring the engine up without a driver: the app's own start, so the
    /// WebView's devtools socket exists before anything asks for it.
    pub async fn ensure(&self) -> Result<(), String> {
        self.0
            .run_mobile_plugin_async::<()>("ensureEngine", ())
            .await
            .map_err(|err| err.to_string())
    }

    /// Raise the takeover: the hand-off's Open.
    pub async fn show(&self) -> Result<(), String> {
        self.0.run_mobile_plugin_async::<()>("show", ()).await.map_err(|err| err.to_string())
    }

    /// Lower the takeover: Done, Not now, the bar's back, the hardware Back.
    pub async fn hide(&self) -> Result<(), String> {
        self.0.run_mobile_plugin_async::<()>("hide", ()).await.map_err(|err| err.to_string())
    }

    /// Whether the takeover is up, for the strip's show/hide button.
    pub async fn windowed(&self) -> Result<bool, String> {
        let answer: Windowed = self
            .0
            .run_mobile_plugin_async("windowed", ())
            .await
            .map_err(|err| err.to_string())?;
        Ok(answer.windowed)
    }
}

/// Register the Kotlin browser plugin, and build the host around its handle.
/// Android is always Wry, so the plugin is pinned to it rather than generic:
/// the host holds the concrete handle.
pub fn init() -> TauriPlugin<tauri::Wry> {
    Builder::new("androidbrowser")
        .setup(|app, api| {
            let handle = api.register_android_plugin("dev.vedhavyas.forge", "BrowserPlugin")?;
            let engine = std::sync::Arc::new(Engine(
                handle,
                std::sync::atomic::AtomicU64::new(0),
            ));
            let boot = std::sync::Arc::clone(&engine);
            let host = match super::StackPaths::android(app) {
                Ok(paths) => {
                    tauri_plugin_log::log::info!("browser engine at {}", paths.stack.display());
                    super::BrowserHost::android(paths, engine)
                }
                Err(why) => {
                    tauri_plugin_log::log::warn!("browser host unavailable: {why}");
                    super::BrowserHost::unavailable(why)
                }
            };
            app.manage(std::sync::Arc::new(host));
            // **The engine comes up with the app**, so the WebView's devtools
            // socket is there before any session asks for it (the desktop's
            // `host.start()` does the same for its browser). Spawned rather
            // than awaited - the window does not wait on a browser.
            tauri::async_runtime::spawn(async move {
                if let Err(why) = boot.ensure().await {
                    tauri_plugin_log::log::warn!("the browser engine did not come up: {why}");
                }
            });
            Ok(())
        })
        .build()
}
