//! The passkey veto the client installs on a host's page.
//!
//! **A host is driven by an agent, not a person.** Nobody is at the machine to
//! answer a passkey prompt, and the engine's dialog window lands on the
//! person's desktop; the page's two WebAuthn calls refuse instead. The veto is
//! a CDP init script on the profile's page target: the target owns the page
//! for the profile's lifetime, so the script survives navigations and a driver
//! rebuild, and it is in place before any page script runs.

use serde_json::json;

use super::spawn;
use crate::browser::window::Cdp;

/// The page-side veto every document starts with: WebAuthn's two request
/// methods refuse instead of raising the engine's passkey dialog.
///
/// Only `get` and `create` are replaced, and a context without
/// `navigator.credentials` (a non-secure page) is left alone. Idempotent: the
/// properties are non-configurable, so a second application leaves the first
/// rejection in place.
pub const PASSKEY_VETO: &str = r#"
(function () {
  try {
    var container = navigator.credentials;
    if (!container) return;
    var deny = function () {
      return Promise.reject(
        new DOMException('WebAuthn is disabled in this browser', 'NotAllowedError'));
    };
    Object.defineProperty(container, 'get', { value: deny, configurable: false, writable: false });
    Object.defineProperty(container, 'create', { value: deny, configurable: false, writable: false });
  } catch (e) {}
})();
"#;

/// Register the veto on `port`'s page and hold it for the host's lifetime.
///
/// **The socket is the registration's lifetime.** A target's init scripts live
/// with the CDP session that added them, so this task keeps its connection
/// open while the host serves the page; a host that exits drops it, and the
/// task with it.
pub async fn install(port: u16) {
    let Some(ws_url) = spawn::page_websocket(port).await else {
        return;
    };
    let prefix = spawn::ws_prefix(port);
    let Some(path) = ws_url.strip_prefix(&prefix) else {
        return;
    };
    let Ok((mut cdp, mut events)) = Cdp::connect_watch(port, path).await else {
        return;
    };
    if let Err(why) = cdp.call("Page.enable", json!({}), None).await {
        tauri_plugin_log::log::info!(
            "the passkey veto could not enable the page (event_name cef_passkey_veto_failed): {why}"
        );
        return;
    }
    if let Err(why) = cdp
        .call("Page.addScriptToEvaluateOnNewDocument", json!({ "source": PASSKEY_VETO }), None)
        .await
    {
        tauri_plugin_log::log::info!(
            "the passkey veto could not be registered (event_name cef_passkey_veto_failed): {why}"
        );
        return;
    }
    // **A page already open** (a carried page at launch) is patched once; the
    // script is idempotent and a no-op where the API is absent.
    let _ = cdp.call("Runtime.evaluate", json!({ "expression": PASSKEY_VETO }), None).await;
    while events.recv().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::PASSKEY_VETO;

    /// **The veto rejects; it never resolves.** The live proof is a driven
    /// host; this pins the script's shape so a reword cannot quietly turn the
    /// refusal into something a page would misread.
    #[test]
    fn the_passkey_veto_rejects_with_not_allowed() {
        assert!(PASSKEY_VETO.contains("Promise.reject"), "the veto rejects");
        assert!(PASSKEY_VETO.contains("'NotAllowedError'"), "the error a page handles");
        assert!(PASSKEY_VETO.contains("'get'"), "the assertion method");
        assert!(PASSKEY_VETO.contains("'create'"), "the registration method");
    }
}
