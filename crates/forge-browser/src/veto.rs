//! The passkey veto installed on a profile's pages.
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
//! every tab blank, the service waits for the target to be armed, and only
//! then is a URL navigated.
//!
//! **A page's popup cannot be one of those paths.** Measured: a popup whose
//! target an auto-attached client holds refuses every page-domain command
//! while it has no document, so no arm can arrive before its first one; and
//! it commits exactly one navigation, so the address it asks for cannot be
//! preceded by a blank. Its first document therefore runs unpatched and is
//! patched in place, as any target some other client creates directly at a
//! URL is.
//!
//! **The event loop waits on no page-domain reply except the arm's.** A
//! target that starts while one is pending - a service worker above all - is
//! paused by this installer's own auto-attach, and the loop is the only thing
//! that can release it: a wait there outlasts the document and wedges the
//! page for good (measured 2026-10-23, #2534). Every context patch is sent,
//! and the release with them; arming is the one place a reply is awaited,
//! because the script's registration is its verdict.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::spawn;
use crate::window::Cdp;

/// The longest one CDP call is given before the install treats it as failed.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

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
/// every indirect call goes through. The return value says whether the patch
/// took (`patched`), was already there (`present`) or hit a frozen prototype
/// (`failed`), so a context is never patched silently.
pub const PASSKEY_VETO: &str = r"
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
";

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
    tracing::warn!(
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
///
/// **Sent, not awaited.** A target waiting for its debugger does not answer
/// the command that releases it (measured 2026-10-23: the call never
/// answered for a page session), while the release itself is processed; an
/// await here would stall the whole event loop on the very target it is
/// trying to free (#2534). **The refusal still gets its line**: the reply is
/// watched from a task of its own, so a release the engine refused is named
/// without the loop waiting for it.
async fn resume(cdp: &mut Cdp, session: &str, port: u16, profile: &Path) {
    let profile = profile.to_path_buf();
    match cdp.send_watch("Runtime.runIfWaitingForDebugger", json!({}), Some(session)).await {
        Ok(answer) => {
            tokio::spawn(async move {
                if let Ok(reply) = answer.await
                    && let Some(error) = reply.get("error")
                {
                    failed("Runtime.runIfWaitingForDebugger", port, &profile, &error.to_string());
                }
            });
        }
        Err(why) => failed("Runtime.runIfWaitingForDebugger", port, &profile, &why),
    }
}

/// Where a target's init script is registered, when it has a document to
/// start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// A page answers on its own attached session.
    Session,
    /// CEF reports a fresh tab as `other`; its page session answers once it
    /// runs.
    PageSession,
}

/// What one attached target gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachPlan {
    /// Whether the target is resumed first. **A paused target that is not
    /// resumed is a worker or a page the browser waits on for ever**:
    /// measured 2026-10-23, a `service_worker` left paused by this installer
    /// stopped a carried navigation and every renderer-side CDP command for
    /// the host's whole life, which the driver then reported as a bare 30 s
    /// `initializeServer` timeout (#2534). So every paused target is
    /// resumed, whether or not it is one this veto arms.
    resume: bool,
    /// Where the script goes, when the target has a document to start.
    arm: Option<Arm>,
}

/// Whether an attached target can be armed on this report.
///
/// **A fresh tab with no document is not armable yet.** A page's popup
/// reports as `other` with an empty url until its first document starts, and
/// every page-domain call on that session times out: an arm attempt holds
/// the installer's loop while other targets wait, and names a failure that is
/// not one. It is armed on its page report, once it has a document.
fn can_arm(kind: &str, url: &str) -> bool {
    !(kind == "other" && url.is_empty())
}

/// The plan for one attached target: resume whatever is paused, and arm only
/// a page or a fresh tab - a worker, an iframe or the engine's own UI has no
/// document of its own to start.
fn attach_plan(kind: &str, paused: bool) -> AttachPlan {
    let arm = match kind {
        "page" => Some(Arm::Session),
        "other" => Some(Arm::PageSession),
        _ => None,
    };
    AttachPlan { resume: paused, arm }
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

/// Evaluate the veto once in one reported context, **watching the verdict
/// from a task of its own**.
///
/// **The loop must stay free to release the next paused target.** A
/// page-domain reply can block for its whole bound while the renderer is
/// starting a document, and a worker that attaches behind that wait is
/// paused by this installer's own auto-attach: releasing it late leaves the
/// page waiting on it for good (measured 2026-10-23, #2534). The verdict is
/// still read, so a frozen prototype or a refused evaluation is named - the
/// watch keeps the signal without the wait.
async fn patch_context(cdp: &mut Cdp, event: &Value, context: &Value, port: u16, profile: &Path) {
    if context["auxData"]["isDefault"].as_bool() != Some(true) {
        return;
    }
    let Some(id) = context["id"].as_i64() else { return };
    let session = event["sessionId"].as_str().unwrap_or_default().to_owned();
    tracing::debug!(
        "the passkey veto patches a context (event_name cef_passkey_veto_context, port {port}, \
         session {session:?}, context {id})"
    );
    let profile = profile.to_path_buf();
    match cdp
        .send_watch(
            "Runtime.evaluate",
            json!({ "expression": PASSKEY_VETO, "contextId": id, "returnByValue": true }),
            Some(&session),
        )
        .await
    {
        Ok(answer) => {
            tokio::spawn(async move {
                let Ok(reply) = answer.await else { return };
                if let Some(error) = reply.get("error") {
                    let why = error.to_string();
                    if context_gone(&why) {
                        tracing::debug!(
                            "the passkey veto skipped a context that went away (event_name \
                             cef_passkey_veto_context_gone, port {port}, profile {}): {why}",
                            profile.display()
                        );
                    } else {
                        failed("Runtime.evaluate", port, &profile, &why);
                    }
                    return;
                }
                let verdict = reply["result"]["result"]["value"].as_str().unwrap_or_default();
                tracing::debug!(
                    "the passkey veto evaluated a context (event_name cef_passkey_veto_verdict, \
                     port {port}, context {id}, verdict {verdict:?}, answer {reply})"
                );
                if verdict.starts_with("failed") {
                    failed("Runtime.evaluate", port, &profile, verdict);
                }
            });
        }
        Err(why) => failed("Runtime.evaluate", port, &profile, &why),
    }
}

/// Attach at `port`'s browser and pause every page target until it is patched.
///
/// **The call answers once the pause is armed; the connection then lives with
/// the host.** A target's init scripts live with the CDP session that added
/// them, so the event loop is the registration's lifetime. An `Err` means no
/// pause was established and nothing is protected.
pub async fn install(port: u16, profile: &Path) -> Result<(), Failure> {
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
    tokio::spawn(async move {
        // **One target, one patch; one context, one evaluate.** A repeated
        // report would register the script twice; the script is idempotent,
        // but the conversation should not be.
        let mut armed: HashSet<String> = HashSet::new();
        let mut attempted: HashMap<String, u8> = HashMap::new();
        let mut patched: HashSet<(String, i64)> = HashSet::new();
        while let Some(event) = events.recv().await {
            match event["method"].as_str() {
                Some("Target.attachedToTarget") => {
                    let session =
                        event["params"]["sessionId"].as_str().unwrap_or_default().to_owned();
                    let target =
                        event["params"]["targetInfo"]["targetId"].as_str().unwrap_or_default();
                    let kind = event["params"]["targetInfo"]["type"].as_str().unwrap_or_default();
                    let url = event["params"]["targetInfo"]["url"].as_str().unwrap_or_default();
                    let paused = event["params"]["waitingForDebugger"].as_bool() == Some(true);
                    let plan = attach_plan(kind, paused);
                    tracing::debug!(
                        "the passkey veto has a target (event_name cef_passkey_veto_attached, \
                         port {port}, session {session}, type {kind}, paused {paused}, arm {:?})",
                        plan.arm
                    );
                    // **Every paused target is released, before the guard
                    // below and whatever its kind.** A target re-reported
                    // with a fresh session - the same page after a
                    // navigation - is paused for that session too, and a
                    // guard that dropped the report would leave the page
                    // waiting for ever. The guard stops a second ARM, not a
                    // second release.
                    //
                    // **A paused target's sessions answer no page-domain
                    // command on this engine** (measured: `Page.enable` and
                    // `Page.addScriptToEvaluateOnNewDocument` both time out
                    // while the target waits). So the pause is released
                    // first, and the script is registered for every document
                    // after the one already under way; its existing world is
                    // patched in place when the context is reported.
                    if plan.resume {
                        resume(&mut cdp, &session, port, &profile).await;
                    }
                    // **One arm per TARGET, not per session**, and **only a
                    // target that has one.** `Target.attachToTarget` raises
                    // another attach report for the same target, so a
                    // session-keyed guard re-attaches for ever; a target
                    // whose arm failed is tried again on its next report
                    // rather than marked here, because a popup's page report
                    // is the one that can be armed.
                    if armed.contains(target) {
                        continue;
                    }
                    // **Only a page or a fresh tab has a document to start.**
                    // A worker, an iframe or the engine's own UI is not a
                    // page: arming one earns a refused `Page.enable` per
                    // domain and names a failure that is not one (rule 20).
                    // It is resumed above all the same - see [`AttachPlan`].
                    let Some(choice) = plan.arm else {
                        tracing::debug!(
                            "the passkey veto skipped a target with no document to start \
                             (event_name cef_passkey_veto_skipped, port {port}, type {kind})"
                        );
                        continue;
                    };
                    // **A fresh tab with no document cannot be armed yet.**
                    // A page's popup reports as `other` with an empty url
                    // until its first document starts, and every page-domain
                    // call on that session times out: the attempt holds this
                    // loop while other targets wait - the stall #2534
                    // forbids - and each timeout names a failure that is not
                    // one. It is armed on its page report, once it has a
                    // document.
                    if !can_arm(kind, url) {
                        tracing::debug!(
                            "the passkey veto waits for a target's first document \
                             (event_name cef_passkey_veto_unstarted, port {port}, type {kind})"
                        );
                        continue;
                    }
                    // **A fresh tab that failed twice is not tried again.** A
                    // `PageSession` arm attaches, and that attach raises its
                    // own report for the same target: without a bound, one
                    // tab that cannot be armed would re-arm itself without
                    // end. A page's own session arm never re-attaches, so its
                    // failures are left to its next report.
                    if choice == Arm::PageSession {
                        let attempts = attempted.entry(target.to_owned()).or_default();
                        if *attempts >= 2 {
                            tracing::debug!(
                                "the passkey veto gave a tab up (event_name \
                                 cef_passkey_veto_unarmable, port {port}, type {kind})"
                            );
                            continue;
                        }
                        *attempts += 1;
                    }
                    let arm_on = match choice {
                        Arm::Session => session,
                        Arm::PageSession => page_session(&mut cdp, target).await.unwrap_or(session),
                    };
                    let registered = arm(&mut cdp, &arm_on, port, &profile).await;
                    if registered {
                        armed.insert(target.to_owned());
                        armed_on
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(target.to_owned());
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
        tracing::debug!(
            "the passkey veto ended (event_name cef_passkey_veto_ended, port {port}, profile {})",
            profile.display(),
        );
        armed_targets().lock().unwrap_or_else(PoisonError::into_inner).remove(&port);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Arm, PASSKEY_VETO, attach_plan, can_arm};

    /// **A target with no document is not armed on that report.** Its
    /// session answers no page-domain command, so the attempt holds the
    /// installer's loop and names a failure that is not one; the popup's
    /// page report is the one that carries a document to arm.
    #[test]
    fn a_fresh_tab_with_no_document_is_not_armed_yet() {
        assert!(!can_arm("other", ""), "a popup with no document waits for its page report");
        assert!(can_arm("other", "about:blank"), "a fresh tab that has a document is armable");
        assert!(can_arm("page", ""), "a page is armable whatever url its report carries");
        assert!(can_arm("page", "https://a/"), "a page with a url is armable");
    }

    /// **A paused target is resumed whatever it is.** The veto arms only a
    /// page and a fresh tab, but CEF pauses every target the auto-attach
    /// reports; a worker left paused wedges every page that needs it -
    /// measured 2026-10-23, a paused `service_worker` stopped a document's
    /// load and the driver's attach for the host's whole life, which surfaced
    /// as a bare 30 s `initializeServer` timeout (#2534). There is no kind
    /// and paused-ness pair for which the plan may leave a target waiting for
    /// a debugger that never comes.
    #[test]
    fn a_paused_target_is_resumed_whatever_it_is() {
        for kind in ["page", "other", "service_worker", "worker", "iframe", "browser", ""] {
            assert!(attach_plan(kind, true).resume, "a paused {kind:?} target must be resumed");
            assert!(!attach_plan(kind, false).resume, "a running {kind:?} target is not resumed");
        }
        assert_eq!(attach_plan("page", false).arm, Some(Arm::Session), "a page answers itself");
        assert_eq!(
            attach_plan("other", true).arm,
            Some(Arm::PageSession),
            "a fresh tab is armed on its page session",
        );
        for kind in ["service_worker", "worker", "iframe", "browser", ""] {
            assert_eq!(attach_plan(kind, true).arm, None, "a {kind:?} target gets no init script");
        }
    }

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
