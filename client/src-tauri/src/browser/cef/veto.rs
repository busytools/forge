//! The passkey veto the client installs on a profile's pages.
//!
//! **A host is driven by an agent, not a person.** Nobody is at the machine to
//! answer a passkey prompt, and the engine's dialog window lands on the
//! person's desktop; the page's WebAuthn entry points refuse instead.
//!
//! The veto attaches at the **browser** endpoint, not a single page target: a
//! profile can hold more than one target (`browser_tabs new`, a scripted
//! `newPage()`), and a veto that covers only the first would leave the rest
//! free to raise the dialog. Every page target is armed as it attaches, and
//! every existing execution context is patched as it is reported, so a page
//! already open - and an iframe already inside it - is covered too.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{Value, json};

use super::spawn;
use crate::browser::window::Cdp;

/// How many times the browser endpoint is asked for before the install is
/// reported failed. The task starts with the host, whose endpoint answers a
/// moment later.
const RESOLVE_TRIES: usize = 20;
const RESOLVE_EVERY: Duration = Duration::from_millis(250);

/// The page-side veto every document starts with: WebAuthn's request methods
/// refuse instead of raising the engine's passkey dialog.
///
/// **Defined on the prototype, not on the instance.** An own property leaves
/// `CredentialsContainer.prototype.get` native, and a caller reaching it that
/// way still raises the dialog; the prototype is what every container and
/// every indirect call goes through. A context without the interface (a
/// non-secure page) is left alone, and the definition is idempotent because
/// the property is non-configurable.
pub const PASSKEY_VETO: &str = r#"
(function () {
  try {
    if (typeof CredentialsContainer === 'undefined') return;
    var proto = CredentialsContainer.prototype;
    var deny = function () {
      return Promise.reject(
        new DOMException('WebAuthn is disabled in this browser', 'NotAllowedError'));
    };
    Object.defineProperty(proto, 'get', { value: deny, configurable: false, writable: false });
    Object.defineProperty(proto, 'create', { value: deny, configurable: false, writable: false });
  } catch (e) {}
})();
"#;

/// One failure line, whatever the stage: a host left without its veto is a
/// real problem and must be visible in the client log.
fn failed(stage: &str, why: &str) {
    tauri_plugin_log::log::warn!(
        "the passkey veto did not install (event_name cef_passkey_veto_failed, stage {stage}): \
         {why}"
    );
}

/// Arm one page target: watch its dialogs, report its contexts, and register
/// the veto before any of its documents run.
async fn arm(cdp: &mut Cdp, session: &str) {
    if let Err(why) = cdp.call("Page.enable", json!({}), Some(session)).await {
        failed("Page.enable", &why);
    }
    // Contexts are reported only once Runtime is enabled; the events are what
    // catch documents (and iframes) that already exist.
    if let Err(why) = cdp.call("Runtime.enable", json!({}), Some(session)).await {
        failed("Runtime.enable", &why);
    }
    if let Err(why) = cdp
        .call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": PASSKEY_VETO }),
            Some(session),
        )
        .await
    {
        failed("Page.addScriptToEvaluateOnNewDocument", &why);
    }
}

/// Patch one reported context when it is a page's or a frame's main world.
async fn patch_context(cdp: &mut Cdp, event: &Value, session: Option<&str>) {
    let context = &event["params"]["context"];
    if context["auxData"]["isDefault"].as_bool() != Some(true) {
        return;
    }
    let Some(id) = context["id"].as_i64() else { return };
    if let Err(why) = cdp
        .call("Runtime.evaluate", json!({ "expression": PASSKEY_VETO, "contextId": id }), session)
        .await
    {
        failed("Runtime.evaluate", &why);
    }
}

/// The browser endpoint, asked for until it answers or the tries run out.
async fn browser_endpoint(port: u16) -> Option<String> {
    for _ in 0..RESOLVE_TRIES {
        if let Some(url) = spawn::browser_websocket(port).await {
            return Some(url);
        }
        tokio::time::sleep(RESOLVE_EVERY).await;
    }
    None
}

/// Register the veto on every page target of `port`'s browser, and hold the
/// registration for the host's lifetime.
///
/// **The socket is the registration's lifetime.** A target's init scripts live
/// with the CDP session that added them, so this task keeps its connection
/// open while the host serves its pages; a host that exits drops it.
pub async fn install(port: u16) {
    let Some(ws_url) = browser_endpoint(port).await else {
        failed("browser endpoint", "the browser endpoint did not answer");
        return;
    };
    let prefix = spawn::ws_prefix(port);
    let Some(path) = ws_url.strip_prefix(&prefix) else {
        failed("browser endpoint", "the browser endpoint is not on the host's port");
        return;
    };
    let Ok((mut cdp, mut events)) = Cdp::connect_watch(port, path).await else {
        failed("browser endpoint", "the browser endpoint did not accept");
        return;
    };
    // **A new target waits for us before it runs.** That is what closes the
    // gap between a target being created and its veto being registered.
    if let Err(why) = cdp
        .call(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            None,
        )
        .await
    {
        // Discovery below still covers new targets, with a short window.
        tauri_plugin_log::log::warn!(
            "the passkey veto could not auto-attach new targets (event_name \
             cef_passkey_veto_failed, stage Target.setAutoAttach): {why}"
        );
    }
    if let Err(why) = cdp.call("Target.setDiscoverTargets", json!({ "discover": true }), None).await
    {
        failed("Target.setDiscoverTargets", &why);
        return;
    }
    let mut armed: HashSet<String> = HashSet::new();
    // The targets that existed when the veto arrived are the profile's own
    // page; the ones that appear afterwards are tabs an agent opened. A later
    // page has already run its first document by the time CEF reports it, so
    // it is reloaded under the registered script; the page that was there
    // first is patched in place instead, because a reload would throw away
    // what the person was reading.
    let settle = tokio::time::sleep(Duration::from_millis(500));
    tokio::pin!(settle);
    let mut settled = false;
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            () = &mut settle, if !settled => { settled = true; continue; }
        };
        let Some(event) = event else { break };
        match event["method"].as_str() {
            Some("Target.attachedToTarget") => {
                let params = &event["params"];
                let session = params["sessionId"].as_str().unwrap_or_default();
                let target = &params["targetInfo"];
                let paused = params["waitingForDebugger"].as_bool() == Some(true);
                let kind = target["type"].as_str().unwrap_or_default();
                let id = target["targetId"].as_str().unwrap_or_default();
                // **A target CEF created late reports itself as `other`.**
                // Its page session comes from a second attach on the same
                // target, and Page.* answers on that one; arming it while the
                // target is still paused is what keeps its first document
                // from running unvetoed.
                let page_session = if kind == "page" {
                    Some(session.to_owned())
                } else if kind == "other" && paused && !id.is_empty() {
                    match cdp
                        .call(
                            "Target.attachToTarget",
                            json!({ "targetId": id, "flatten": true }),
                            None,
                        )
                        .await
                    {
                        Ok(answer) => answer["sessionId"].as_str().map(str::to_owned),
                        Err(why) => {
                            failed("Target.attachToTarget", &why);
                            None
                        }
                    }
                } else {
                    None
                };
                if let Some(page_session) = page_session {
                    let fresh = if id.is_empty() { true } else { armed.insert(id.to_owned()) };
                    if fresh {
                        arm(&mut cdp, &page_session).await;
                        if !paused
                            && settled
                            && let Err(why) =
                                cdp.call("Page.reload", json!({}), Some(&page_session)).await
                        {
                            failed("Page.reload", &why);
                        }
                    }
                }
                // Whatever the target is, never leave it paused.
                let _ = cdp.call("Runtime.runIfWaitingForDebugger", json!({}), Some(session)).await;
            }
            // **A target that already existed is attached here**, and its
            // page session comes back in this call's own answer. A target CEF
            // created late lands here too: it attaches as `other` while
            // paused, and only this second attach gives a session the page
            // domain answers on.
            Some("Target.targetCreated") => {
                let target = &event["params"]["targetInfo"];
                if target["type"].as_str() != Some("page") {
                    continue;
                }
                let Some(id) = target["targetId"].as_str() else { continue };
                let answer = cdp
                    .call("Target.attachToTarget", json!({ "targetId": id, "flatten": true }), None)
                    .await;
                let Ok(answer) = answer else {
                    // Already attached by the auto-attach path, which arms it
                    // from its own event; anything else is a real failure.
                    continue;
                };
                let Some(session) = answer["sessionId"].as_str() else { continue };
                if !armed.insert(id.to_owned()) {
                    continue;
                }
                arm(&mut cdp, session).await;
                // The document may already have run: restart it under the
                // registered script.
                if let Err(why) = cdp.call("Page.reload", json!({}), Some(session)).await {
                    failed("Page.reload", &why);
                }
            }
            Some("Runtime.executionContextCreated") => {
                patch_context(&mut cdp, &event, event["sessionId"].as_str()).await;
            }
            _ => {}
        }
    }
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
        assert!(PASSKEY_VETO.contains("CredentialsContainer.prototype"), "the prototype");
        assert!(PASSKEY_VETO.contains("configurable: false"), "not replaceable");
        assert!(PASSKEY_VETO.contains("'get'"), "the assertion method");
        assert!(PASSKEY_VETO.contains("'create'"), "the registration method");
    }
}
