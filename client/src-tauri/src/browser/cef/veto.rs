//! The passkey veto the client installs on a profile's pages.
//!
//! **A host is driven by an agent, not a person.** Nobody is at the machine to
//! answer a passkey prompt, and the engine's dialog window lands on the
//! person's desktop; the page's WebAuthn entry points refuse instead.
//!
//! **One path, and it is the engine's own pause.** The installer attaches at
//! the browser endpoint and turns on auto-attach with
//! `waitForDebuggerOnStart`, so a target a tool creates later is reported to
//! it: a page that was already running is armed in place, and a fresh tab is
//! released and then armed on its page session, so every document after the
//! one under way starts with the script registered.
//!
//! **This engine answers no page-domain command while a target waits**, so a
//! document that starts with its tab can run before the script is
//! registered. The paths that open documents avoid that: the host creates
//! its browser on `about:blank` and the installer navigates to the carried
//! page after the script is registered there, and the browser tools open a
//! tab at a URL blank first, wait for the arm, and then navigate. A tab some
//! other client creates directly at a URL still runs that first document
//! unpatched; its world is patched in place as soon as the context is
//! reported.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use super::spawn;
use crate::browser::window::Cdp;

/// The longest one CDP call is given before the install treats it as failed.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest the installed veto waits for a fresh host's carried page to
/// announce its navigation before answering without it.
const CARRIED_TIMEOUT: Duration = Duration::from_secs(20);

/// How many times the browser endpoint is asked for before the install is
/// reported failed; the task starts with the host, whose endpoint answers a
/// moment later.
const RESOLVE_TRIES: usize = 20;
const RESOLVE_EVERY: Duration = Duration::from_millis(250);

/// The page-side veto every document starts with: WebAuthn's request methods
/// refuse instead of raising the engine's passkey dialog.
///
/// **Defined on the prototype, not on the instance.** An own property leaves
/// `CredentialsContainer.prototype.get` native, and a caller reaching it that
/// way still raises the dialog; the prototype is what every container and
/// every indirect call goes through. The return value is read by the
/// evaluate that patches an existing context, so a frozen prototype surfaces
/// as `failed` rather than as silence.
pub const PASSKEY_VETO: &str = r#"
(function () {
  try {
    if (typeof CredentialsContainer === 'undefined') return 'absent';
    var proto = CredentialsContainer.prototype;
    if (proto.get && proto.get.__forgeVeto) return 'present';
    var deny = function () {
      return Promise.reject(
        new DOMException('WebAuthn is disabled in this browser', 'NotAllowedError'));
    };
    deny.__forgeVeto = true;
    Object.defineProperty(proto, 'get', { value: deny, configurable: false, writable: false });
    Object.defineProperty(proto, 'create', { value: deny, configurable: false, writable: false });
    return 'patched';
  } catch (e) {
    return 'failed ' + e.name + ': ' + e.message;
  }
})();
"#;

/// A stage the install could not get past.
#[derive(Debug)]
pub struct Failure {
    pub stage: &'static str,
    pub why: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.stage, self.why)
    }
}

/// One failure line, naming the stage, the port and the profile it belongs to.
fn failed(stage: &'static str, port: u16, profile: &Path, why: &str) {
    tauri_plugin_log::log::warn!(
        "the passkey veto did not install (event_name cef_passkey_veto_failed, stage {stage}, \
         port {port}, profile {}): {why}",
        profile.display()
    );
}

/// One bounded CDP call. A call that never answers is a failed call, not a
/// task that waits forever.
async fn call(
    cdp: &mut Cdp,
    method: &str,
    params: Value,
    session: Option<&str>,
) -> Result<Value, String> {
    match tokio::time::timeout(CALL_TIMEOUT, cdp.call(method, params, session)).await {
        Ok(answer) => answer,
        Err(_) => Err(format!("{method} did not answer within {} s", CALL_TIMEOUT.as_secs())),
    }
}

/// The targets each installed host has armed, by port.
///
/// **The browser tools wait on a TARGET id**, not a count: a tab is armed
/// once the installer has registered the script on that very target, and a
/// count would let a previous tab's arm release the next call's wait.
type Armed = Arc<Mutex<HashSet<String>>>;

fn armed_targets() -> &'static Mutex<HashMap<u16, Armed>> {
    static ARMED: std::sync::OnceLock<Mutex<HashMap<u16, Armed>>> = std::sync::OnceLock::new();
    ARMED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn armed_set(port: u16) -> Armed {
    let mut armed = armed_targets().lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(armed.entry(port).or_default())
}

/// Whether the installer has armed `target` on the host at `port`.
pub fn target_armed(port: u16, target: &str) -> bool {
    armed_targets()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&port)
        .is_some_and(|armed| armed.lock().unwrap_or_else(PoisonError::into_inner).contains(target))
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

/// Whether a call failed because its context went away between the report and
/// the patch. That is a page moving on, not a veto that did not install.
fn context_gone(why: &str) -> bool {
    why.contains("Cannot find context") || why.contains("Execution context was destroyed")
}

/// Patch one attached target's session, then leave it to the caller to resume.
///
/// **Every step is attempted even when an earlier one failed**: a target is
/// resumed whatever happened, because one left paused is a page no one can
/// use. Each failure is its own line.
async fn arm(cdp: &mut Cdp, session: &str, port: u16, profile: &Path) -> bool {
    let mut registered = false;
    for (method, params) in [
        ("Page.enable", json!({})),
        ("Runtime.enable", json!({})),
        ("Page.addScriptToEvaluateOnNewDocument", json!({ "source": PASSKEY_VETO })),
    ] {
        match call(cdp, method, params, Some(session)).await {
            Ok(_) => registered |= method == "Page.addScriptToEvaluateOnNewDocument",
            Err(why) => failed(method, port, profile, &why),
        }
    }
    registered
}

/// Release one paused target, naming a failure rather than leaving it frozen.
async fn resume(cdp: &mut Cdp, session: &str, port: u16, profile: &Path) {
    if let Err(why) = call(cdp, "Runtime.runIfWaitingForDebugger", json!({}), Some(session)).await {
        failed("Runtime.runIfWaitingForDebugger", port, profile, &why);
    }
}

/// A page session for a target CEF reported as something else, so the script
/// can be registered once the target runs.
async fn page_session(cdp: &mut Cdp, target: &str) -> Option<String> {
    call(cdp, "Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)
        .await
        .ok()?
        .get("sessionId")?
        .as_str()
        .map(str::to_owned)
}

/// Evaluate the veto once in one reported context and read what it returned.
async fn patch_context(cdp: &mut Cdp, event: &Value, context: &Value, port: u16, profile: &Path) {
    if context["auxData"]["isDefault"].as_bool() != Some(true) {
        return;
    }
    let Some(id) = context["id"].as_i64() else { return };
    let session = event["sessionId"].as_str();
    tauri_plugin_log::log::debug!(
        "the passkey veto patches a context (event_name cef_passkey_veto_context, port {port}, \
         session {session:?}, context {id})"
    );
    let answer = call(
        cdp,
        "Runtime.evaluate",
        json!({ "expression": PASSKEY_VETO, "contextId": id, "returnByValue": true }),
        session,
    )
    .await;
    match answer {
        Ok(answer) => {
            let verdict = answer["result"]["value"].as_str().unwrap_or_default();
            tauri_plugin_log::log::debug!(
                "the passkey veto evaluated a context (event_name cef_passkey_veto_verdict, port \
                 {port}, session {session:?}, context {id}, verdict {verdict:?}, answer {answer})"
            );
            if verdict.starts_with("failed") {
                failed("Runtime.evaluate", port, profile, verdict);
            }
        }
        Err(why) if context_gone(&why) => {
            tauri_plugin_log::log::debug!(
                "the passkey veto skipped a context that went away (event_name \
                 cef_passkey_veto_context_gone, port {port}, profile {}): {why}",
                profile.display()
            );
        }
        Err(why) => failed("Runtime.evaluate", port, profile, &why),
    }
}

/// What became of the page a fresh host was asked to open second.
#[derive(Debug, PartialEq, Eq)]
pub enum Carried {
    /// Nothing was carried: the host stays on `about:blank`.
    None,
    /// The carried navigation was ACCEPTED behind the veto. **Not the same
    /// as landed**: the target's URL may still read `about:blank` for a
    /// moment, which is why the carry stays held until a raise reads a real
    /// page.
    Opened,
    /// The navigation failed; the host serves blank and the reason is named.
    Failed(String),
}

/// Attach at `port`'s browser and pause every page target until it is patched.
///
/// **The call answers once the pause is armed; the connection then lives with
/// the host.** A target's init scripts live with the CDP session that added
/// them, so the event loop is the registration's lifetime. An `Err` means no
/// pause was established and nothing is protected.
///
/// **`carried` is the page a fresh host opens second.** The host starts on
/// `about:blank`; the first page target it reports is that blank document,
/// and once the script is registered on it the installer navigates it to the
/// carried page. An adopted host passes `None`: its page is the person's.
pub async fn install(
    port: u16,
    profile: &Path,
    carried: Option<String>,
) -> Result<Carried, Failure> {
    let ws_url = browser_endpoint(port).await.ok_or(Failure {
        stage: "browser endpoint",
        why: "the endpoint did not answer".to_owned(),
    })?;
    let prefix = spawn::ws_prefix(port);
    let Some(path) = ws_url.strip_prefix(&prefix) else {
        return Err(Failure {
            stage: "browser endpoint",
            why: "the endpoint is not on the host's port".to_owned(),
        });
    };
    let (mut cdp, mut events) = Cdp::connect_watch(port, path)
        .await
        .map_err(|why| Failure { stage: "browser endpoint", why })?;
    // **The pause is the coverage.** A target that is already running is
    // patched in place by the context handler below; a target that appears
    // later stops here first, whatever its type. **`flatten` is not
    // optional here**: the engine refuses browser-level auto-attach without
    // it, and every child call below is addressed by its session id.
    call(
        &mut cdp,
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        None,
    )
    .await
    .map_err(|why| Failure { stage: "Target.setAutoAttach", why })?;
    let profile = profile.to_path_buf();
    let armed_on = armed_set(port);
    let asked = carried.is_some();
    let (report, outcome) = tokio::sync::oneshot::channel::<Result<(), String>>();
    tauri::async_runtime::spawn(async move {
        let mut report = Some(report);
        // **One target, one patch; one context, one evaluate.** A repeated
        // report would register the script twice; the script is idempotent,
        // but the conversation should not be.
        let mut armed: HashSet<String> = HashSet::new();
        let mut patched: HashSet<(String, i64)> = HashSet::new();
        let mut carried = carried;
        while let Some(event) = events.recv().await {
            match event["method"].as_str() {
                Some("Target.attachedToTarget") => {
                    let session =
                        event["params"]["sessionId"].as_str().unwrap_or_default().to_owned();
                    let target =
                        event["params"]["targetInfo"]["targetId"].as_str().unwrap_or_default();
                    // **One arm per TARGET, not per session.**
                    // `Target.attachToTarget` raises another attach report for
                    // the same target, so a session-keyed guard re-attaches
                    // for ever.
                    if !armed.insert(target.to_owned()) {
                        continue;
                    }
                    let kind = event["params"]["targetInfo"]["type"].as_str().unwrap_or_default();
                    // **Only a page or a fresh tab has a document to start.**
                    // A worker, an iframe or the engine's own UI is not a
                    // page: arming one earns a refused `Page.enable` per
                    // domain and names a failure that is not one (rule 20).
                    if kind != "page" && kind != "other" {
                        tauri_plugin_log::log::debug!(
                            "the passkey veto skipped a non-page target (event_name \
                             cef_passkey_veto_skipped, port {port}, type {kind})"
                        );
                        continue;
                    }
                    let paused = event["params"]["waitingForDebugger"].as_bool() == Some(true);
                    // **A paused target's sessions answer no page-domain
                    // command on this engine** (measured: `Page.enable` and
                    // `Page.addScriptToEvaluateOnNewDocument` both time out
                    // while the target waits). So the pause is released
                    // first, and the script is registered for every document
                    // after the one already under way; its existing world is
                    // patched in place when the context is reported.
                    tauri_plugin_log::log::debug!(
                        "the passkey veto has a target (event_name cef_passkey_veto_attached, \
                         port {port}, session {session}, type {kind}, paused {paused})"
                    );
                    if paused {
                        resume(&mut cdp, &session, port, &profile).await;
                    }
                    let arm_on = if kind == "page" {
                        session
                    } else {
                        // CEF reports a fresh tab as `other`; the page
                        // session is the one that answers once it runs.
                        page_session(&mut cdp, target).await.unwrap_or(session)
                    };
                    let registered = arm(&mut cdp, &arm_on, port, &profile).await;
                    if registered {
                        armed_on
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(target.to_owned());
                    }
                    // **A carried navigation waits for a page that can carry
                    // it.** A first page whose script did not register is
                    // reported at once, rather than waited out for the bound.
                    if !registered && kind == "page" && carried.is_some() {
                        if let Some(report) = report.take() {
                            let _ =
                                report
                                    .send(Err("the script was not registered on the first page"
                                        .to_owned()));
                        }
                        carried = None;
                    }
                    // **The carried page opens behind the veto**, and the
                    // install answers on its outcome: the host is not handed
                    // to a raise until this document is the one on screen,
                    // so a probe cannot mistake the blank page for the carry.
                    // The fail-fast branch above has already consumed
                    // `carried` for a page whose script did not register, so
                    // this only runs for a page that can carry the
                    // navigation.
                    if kind == "page"
                        && let Some(url) = carried.take()
                    {
                        let result =
                            call(&mut cdp, "Page.navigate", json!({ "url": url }), Some(&arm_on))
                                .await;
                        let result = match result {
                            Ok(_) => Ok(()),
                            Err(why) => {
                                failed("Page.navigate", port, &profile, &why);
                                Err(why)
                            }
                        };
                        if let Some(report) = report.take() {
                            let _ = report.send(result);
                        }
                    }
                }
                Some("Runtime.executionContextCreated") => {
                    let context = &event["params"]["context"];
                    let session = event["sessionId"].as_str().unwrap_or_default();
                    let Some(id) = context["id"].as_i64() else { continue };
                    if !patched.insert((session.to_owned(), id)) {
                        continue;
                    }
                    patch_context(&mut cdp, &event, context, port, &profile).await;
                }
                _ => {}
            }
        }
        // **The socket is gone, so the pause is gone with it.** Name the end
        // rather than exit silently, and drop this port's armed set: a wait
        // that still answered here would be answering for a veto that no
        // longer exists.
        tauri_plugin_log::log::debug!(
            "the passkey veto ended (event_name cef_passkey_veto_ended, port {port}, profile {})",
            profile.display(),
        );
        armed_targets().lock().unwrap_or_else(PoisonError::into_inner).remove(&port);
    });
    if !asked {
        return Ok(Carried::None);
    }
    match tokio::time::timeout(CARRIED_TIMEOUT, outcome).await {
        Ok(Ok(Ok(()))) => Ok(Carried::Opened),
        Ok(Ok(Err(why))) => Ok(Carried::Failed(why)),
        Ok(Err(_)) => {
            Ok(Carried::Failed("the installer stopped before the carried page opened".to_owned()))
        }
        Err(_) => Ok(Carried::Failed(format!(
            "the carried page did not open within {} s",
            CARRIED_TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(feature = "testing")]
pub mod testing;

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
        assert!(PASSKEY_VETO.contains("return 'failed"), "a frozen prototype says so");
    }
}
