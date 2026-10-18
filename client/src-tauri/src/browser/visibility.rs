//! The page's visibility: what Chromium freezes when a page reports itself
//! hidden, and what a browser call says about it.
//!
//! **A hidden page is not a wedged browser (#2385).** Chromium stops a hidden
//! page's frames, so Playwright's actionability gate - which waits for the
//! target to be stable across frames - never finishes, while `page.evaluate`
//! still reads the DOM: that wait dies at its own timeout, a rAF-driven menu
//! never mounts, and a mouse write can hold the driver to its call bound.
//! **The gate is exactly the calls that take that wait**: pointer actions
//! without `force` (a forced click skips the actionability gate entirely),
//! and a `fill_form` carrying a checkbox or radio, which Playwright sets with
//! a click. Key input is not gated: a hidden page drops it silently and
//! Playwright's press path never checks stability, so those calls return.
//!
//! The gated calls ask the page's `document.visibilityState` first and are
//! refused by name; `browser_snapshot` still answers and carries the state;
//! and a call that failed anyway is re-probed, so a page that went hidden
//! mid-call is diagnosed too. **Nothing here raises the tab**: the person's
//! focus is never taken, and every refusal names the remedy instead.
//!
//! **Desktop only.** The remedy is `page.bringToFront`, and it cannot make a
//! phone's WebView visible - the phone also keeps that WebView in an
//! invisible holder while no takeover is up, where this would fire on the
//! normal flow and the remedy could loop. The phone keeps the behaviour it
//! had.
//!
//! **The probe costs one driver round trip, and upstream's own settle.** The
//! vendored driver waits its `timeouts.settle` (500 ms) after every tool call
//! it runs through its own `waitForCompletion`, `browser_evaluate` included,
//! so each watched call on a visible page pays that once - the watched calls
//! are `browser_snapshot` and the pointer class, and the re-probe runs only
//! after a failure. A CDP read of our own would skip the settle but cannot
//! address the driver's CURRENT tab when the browser holds several - probing
//! the wrong page would refuse a visible tab or drive a hidden one, which is
//! worse than the settle.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::driver::{self, Driver, ReplyPart};
use super::profiles::{routed_call, routing_refusal};

/// The longest the probe is given. A live page answers an evaluate in
/// milliseconds; a probe that cannot answer inside this is no evidence about
/// the page, and a bound of its own keeps it off the call's.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The probe: one evaluate, the read a hidden page still answers.
const PROBE: &str = "() => ({ visibility: document.visibilityState, url: document.location.href, \
                     title: document.title })";

/// The pointer actions whose actionability gate waits on frames, so a hidden
/// page kills them. `browser_click` is absent because a forced click is the
/// exception; `browser_fill_form` because only its checkbox and radio fields
/// are pointer actions.
const INPUT_TOOLS: &[&str] =
    &["browser_click_and_capture", "browser_drag", "browser_drop", "browser_hover"];

/// A click takes the stability wait unless `force` skips the gate.
const CLICK: &str = "browser_click";

/// `browser_fill_form` fills and selects through the DOM, except a checkbox
/// or radio, which Playwright sets with a pointer action.
const FILL_FORM: &str = "browser_fill_form";

/// The read whose answer carries the state.
const SNAPSHOT: &str = "browser_snapshot";

/// The wait, whose own polling dies on a hidden page's stopped frames.
const WAIT: &str = "browser_wait_for";

/// The escape hatch, and the issue's own `page.mouse.wheel` repro: arbitrary
/// Playwright code cannot be classified up front, so it is diagnosed on its
/// timeout instead.
const RUN_CODE: &str = "browser_run_code_unsafe";

/// A screenshot of an ELEMENT waits on the same stability gate as a click.
const SCREENSHOT: &str = "browser_take_screenshot";

/// The longest a tab's title or URL may be in a message, so one enormous
/// name cannot bury the remedy.
const NAME_LIMIT: usize = 200;

/// The remedy every refusal and diagnosis carries. **The profile is named**:
/// a call made against a named profile that then asks `browser_tabs` without
/// it would drive the shared browser and raise the wrong window.
const REMEDY: &str = "Run browser_tabs with action \"list\" (pass the same `profile` argument this \
                      call carried, if any) to see the tab's index, then browser_tabs with \
                      action \"select\" and that index; the driver runs page.bringToFront on \
                      it. The host never raises the tab itself, so the person's focus is not \
                      taken.";

/// Where the page stands, as the probe found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PageState {
    pub(super) hidden: bool,
    pub(super) url: String,
    pub(super) title: String,
}

/// What the probe's result section carries.
#[derive(Deserialize)]
struct Probe {
    visibility: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
}

/// What one call should do about the state the probe found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Refuse(String),
    Note(String),
    CarryOn,
}

/// One call, with the visibility asked first where it decides and the call
/// re-probed when it fails: a hidden page is refused or diagnosed, never
/// left to read as a wedged driver.
pub(super) async fn dispatch(
    driver: &Driver,
    tool: &str,
    args: &Value,
) -> Result<Vec<ReplyPart>, String> {
    // **The call's own shape comes first**: a malformed call on a hidden page
    // is its argument error, not the visibility gate's.
    if let Some(why) = routing_refusal(tool, args) {
        return Err(why);
    }
    let state = if watched(tool, args) { probe(driver).await } else { None };
    let note = pre_call(tool, args, state.as_ref())?;
    let bound = action_bound(tool, args);
    match routed_call(driver, tool, args, bound).await {
        Ok(mut parts) => {
            if let Some(note) = note {
                parts.push(ReplyPart::Text { text: note });
            }
            Ok(parts)
        }
        Err(why) => Err(after_failure(driver, tool, args, why, bound).await),
    }
}

/// The pre-call half of [`dispatch`], pure and CI-tested: refuse a hidden
/// page's pointer call, carry the snapshot's note, or carry on.
pub(super) fn pre_call(
    tool: &str,
    args: &Value,
    state: Option<&PageState>,
) -> Result<Option<String>, String> {
    match decide(tool, args, state) {
        Decision::Refuse(why) => Err(why),
        Decision::Note(note) => Ok(Some(note)),
        Decision::CarryOn => Ok(None),
    }
}

/// Whether this call takes Playwright's stability wait: a pointer action, a
/// click unless `force` skips the gate, or a checkbox/radio set with a click.
pub(super) fn input_call(tool: &str, args: &Value) -> bool {
    match tool {
        CLICK => args.get("force").and_then(Value::as_bool) != Some(true),
        FILL_FORM => args
            .get("fields")
            .and_then(Value::as_array)
            .is_some_and(|fields| fields.iter().any(pointer_field)),
        _ => INPUT_TOOLS.contains(&tool),
    }
}

/// Whether one `fill_form` field is set with a pointer action rather than a
/// DOM fill or select.
fn pointer_field(field: &Value) -> bool {
    matches!(field.get("type").and_then(Value::as_str), Some("checkbox" | "radio"))
}

/// Whether an element screenshot waits on the stability gate.
fn waits_on_stability(tool: &str, args: &Value) -> bool {
    tool == SCREENSHOT && args.get("target").is_some()
}

/// Whether a call has its visibility asked before it runs.
pub(super) fn watched(tool: &str, args: &Value) -> bool {
    input_call(tool, args) || tool == SNAPSHOT
}

/// Whether a call may be re-probed after a failure.
fn reprobed(tool: &str, args: &Value) -> bool {
    input_call(tool, args) || tool == WAIT || waits_on_stability(tool, args) || tool == RUN_CODE
}

/// Whether a reason is a bound running out: the driver's own sentence, or
/// Playwright's action timeout, which it reports as "Timeout <n>ms
/// exceeded." - the failure a hidden page manufactures.
fn timed_out(why: &str, tool: &str, bound: Duration) -> bool {
    why == driver::did_not_answer(tool, bound) || why.contains("Timeout ")
}

/// Whether a failed call is worth re-probing. A pointer call is re-probed
/// whatever its failure says (a hidden page manufactures a stability
/// timeout); a wait or an element screenshot on its timeout; arbitrary code
/// only on the driver's own bound, because its other failures carry their
/// own reasons.
pub(super) fn needs_reprobe(tool: &str, args: &Value, why: &str, bound: Duration) -> bool {
    if input_call(tool, args) {
        return true;
    }
    if tool == RUN_CODE {
        return why == driver::did_not_answer(tool, bound);
    }
    (tool == WAIT || waits_on_stability(tool, args)) && timed_out(why, tool, bound)
}

/// The bound a call gets. A call that may re-probe keeps PROBE_TIMEOUT below
/// the driver's own bound, so the probe after its failure still fits in it.
pub(super) fn action_bound(tool: &str, args: &Value) -> Duration {
    if reprobed(tool, args) {
        driver::CALL_TIMEOUT.saturating_sub(PROBE_TIMEOUT)
    } else {
        driver::CALL_TIMEOUT
    }
}

/// The decision for one call against the state the probe found. Pure, so the
/// sequence is pinned without a driver; `None` is a probe that could not
/// read, and carries on rather than guessing.
pub(super) fn decide(tool: &str, args: &Value, state: Option<&PageState>) -> Decision {
    let Some(state) = state.filter(|state| state.hidden) else {
        return Decision::CarryOn;
    };
    if input_call(tool, args) {
        return Decision::Refuse(input_refusal(tool, state));
    }
    if tool == SNAPSHOT {
        return Decision::Note(hidden_note());
    }
    Decision::CarryOn
}

/// Read the page's state, or `None` when the driver cannot answer - a page
/// that never loaded, or an evaluate the driver itself refused. `None` is
/// "carry on": a probe that cannot read is not evidence the page is hidden.
pub(super) async fn probe(driver: &Driver) -> Option<PageState> {
    let parts = driver.evaluate(PROBE, PROBE_TIMEOUT).await.ok()?;
    let text = parts.iter().find_map(|part| match part {
        ReplyPart::Text { text } => Some(text.as_str()),
        ReplyPart::Image { .. } => None,
    })?;
    parse_state(text)
}

/// A call failed: re-probe, and name a page that is hidden now.
async fn after_failure(
    driver: &Driver,
    tool: &str,
    args: &Value,
    why: String,
    bound: Duration,
) -> String {
    if !needs_reprobe(tool, args, &why, bound) {
        return why;
    }
    let state = probe(driver).await;
    failure_verdict(tool, args, &why, bound, state.as_ref()).unwrap_or(why)
}

/// The pure half of [`after_failure`], testable without a driver. A call's
/// own reason is kept and the diagnosis appended: the failure is still what
/// happened, and the hidden page is what explains it.
fn failure_verdict(
    tool: &str,
    args: &Value,
    why: &str,
    bound: Duration,
    state: Option<&PageState>,
) -> Option<String> {
    let state = state.filter(|state| state.hidden)?;
    if input_call(tool, args) {
        return Some(format!("{why}\n\n{}", input_refusal(tool, state)));
    }
    let diagnosed = if tool == RUN_CODE {
        why == driver::did_not_answer(tool, bound)
    } else {
        (tool == WAIT || waits_on_stability(tool, args)) && timed_out(why, tool, bound)
    };
    diagnosed.then(|| format!("{why}\n\n{}", hidden_diagnosis(tool, state)))
}

/// The refusal for a pointer call against a hidden page.
fn input_refusal(tool: &str, state: &PageState) -> String {
    format!(
        "{tool} dispatches pointer input and {} reports document.visibilityState \"hidden\": \
         Chromium stops a hidden page's frames, so Playwright's stability wait never finishes - \
         the call would die in it or hold the driver to its bound. {REMEDY} This call can be \
         repeated once the page is visible.",
        tab_name(state),
    )
}

/// The note `browser_snapshot` carries on a hidden page - the same diagnosis
/// on the read that still works, so the answer cannot pass for a wedged page.
fn hidden_note() -> String {
    format!(
        "The page reports document.visibilityState \"hidden\": this read still answers, but the \
         page's frames are stopped and its input dispatch frozen. {REMEDY}"
    )
}

/// The diagnosis appended to a call whose own wait died on a hidden page.
fn hidden_diagnosis(tool: &str, state: &PageState) -> String {
    format!(
        "The tab is hidden: {} reports document.visibilityState \"hidden\", and Chromium stops a \
         hidden page's frames - a stability wait, a mouse write or an animation-frame poll never \
         finishes, which is how {tool} reached its bound. {REMEDY}",
        tab_name(state),
    )
}

/// The tab as a refusal names it: its title and URL when the page has them,
/// each clipped so one enormous name cannot bury the remedy.
fn tab_name(state: &PageState) -> String {
    let title = clipped(state.title.trim());
    let url = clipped(state.url.trim());
    match (title.is_empty(), url.is_empty()) {
        (false, false) => format!("\"{title}\" ({url})"),
        (false, true) => format!("\"{title}\""),
        (true, false) => url,
        (true, true) => "the current tab".to_owned(),
    }
}

/// `text`, clipped to [`NAME_LIMIT`] characters on a character boundary.
fn clipped(text: &str) -> String {
    if text.chars().count() <= NAME_LIMIT {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(NAME_LIMIT).collect();
    out.push_str("...");
    out
}

/// Read the probe's answer: the `### Result` section of the driver's own
/// reply, which is `JSON.stringify` of the returned object. Anything else is
/// `None`, never a guess.
fn parse_state(text: &str) -> Option<PageState> {
    let body = result_section(text)?;
    let probe: Probe = serde_json::from_str(body).ok()?;
    Some(PageState { hidden: probe.visibility != "visible", url: probe.url, title: probe.title })
}

/// The text under `### Result`, up to the next section header.
fn result_section(text: &str) -> Option<&str> {
    let start = text.find("### Result\n")? + "### Result\n".len();
    let rest = &text[start..];
    let end = rest.find("\n### ").unwrap_or(rest.len());
    Some(rest[..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The driver's own answer shape, copied from a measured `browser_evaluate`
    /// reply: the JSON under `### Result`, then the code it ran.
    const HIDDEN: &str = "### Result\n{\n  \"visibility\": \"hidden\",\n  \"url\": \
                          \"data:text/html,<title>Sign in</title>\",\n  \"title\": \"Sign in\"\n}\n\
                          \n### Ran Playwright code\n```js\nawait page.evaluate('() => ({ \
                          visibility: document.visibilityState, url: document.location.href, \
                          title: document.title })');\n```";

    fn hidden() -> PageState {
        PageState {
            hidden: true,
            url: "https://example.com/login".to_owned(),
            title: "Sign in".to_owned(),
        }
    }

    fn visible() -> PageState {
        PageState { hidden: false, ..hidden() }
    }

    fn refusal(tool: &str, args: &Value) -> String {
        match decide(tool, args, Some(&hidden())) {
            Decision::Refuse(why) => why,
            other => panic!("{tool} is not refused on a hidden page: {other:?}"),
        }
    }

    #[test]
    fn the_probe_answer_reads_as_the_pages_state() {
        let state = parse_state(HIDDEN).expect("the measured answer parses");
        assert!(state.hidden, "hidden is the state the freeze is named for");
        assert_eq!(state.title, "Sign in");
        assert_eq!(state.url, "data:text/html,<title>Sign in</title>");

        let shown = HIDDEN.replacen("\"hidden\"", "\"visible\"", 1);
        assert!(!parse_state(&shown).expect("visible parses").hidden);
    }

    /// A reply with no Result section - the driver refused the evaluate, or
    /// answered some other shape - is no evidence about the page.
    #[test]
    fn an_answer_this_probe_cannot_read_is_not_a_hidden_page() {
        assert!(parse_state("### Error\nno page is open").is_none());
        assert!(parse_state("").is_none());
        assert!(parse_state("### Result\nnot json").is_none());
    }

    /// **The refused class is the stability wait and nothing else.** Pointer
    /// actions without `force` die on a hidden page's stopped frames; a
    /// forced click skips the gate, key input is dropped silently and the
    /// press path returns, and the DOM-shaped calls land there. Refusing any
    /// of those would break work that succeeds.
    #[test]
    fn only_the_calls_that_take_the_stability_wait_are_refused() {
        for tool in ["browser_click_and_capture", "browser_drag", "browser_drop", "browser_hover"] {
            assert!(input_call(tool, &json!({})), "{tool} takes the stability wait");
            assert!(watched(tool, &json!({})), "{tool} asks the page's visibility first");
            refusal(tool, &json!({}));
            assert_eq!(decide(tool, &json!({}), Some(&visible())), Decision::CarryOn);
        }

        // A forced click skips checkElementStates outright, so it completes
        // hidden; an absent or false `force` is the plain click.
        let forced = json!({ "target": "#b", "force": true });
        assert!(!input_call(CLICK, &forced), "a forced click takes no stability wait");
        assert!(!watched(CLICK, &forced));
        assert_eq!(decide(CLICK, &forced, Some(&hidden())), Decision::CarryOn);
        for plain in [json!({ "target": "#b" }), json!({ "target": "#b", "force": false })] {
            assert!(input_call(CLICK, &plain), "a plain click takes the stability wait");
            refusal(CLICK, &plain);
            assert_eq!(decide(CLICK, &plain, Some(&visible())), Decision::CarryOn);
        }

        // Key input is dropped silently and Playwright's press path never
        // checks stability: those calls return rather than hang.
        for keyboard in [
            json!({ "key": "Home" }),
            json!({ "target": "e1", "text": "x", "submit": true }),
            json!({ "target": "e1", "text": "x", "slowly": true }),
            json!({ "target": "e1", "text": "x" }),
        ] {
            let tool =
                if keyboard.get("key").is_some() { "browser_press_key" } else { "browser_type" };
            assert!(!input_call(tool, &keyboard), "{tool} takes no stability wait: {keyboard}");
            assert!(!watched(tool, &keyboard));
            assert_eq!(decide(tool, &keyboard, Some(&hidden())), Decision::CarryOn);
        }

        // A `fill_form` is a DOM fill unless a checkbox or radio is among
        // its fields; those are set with a click.
        let text_only = json!({ "fields": [
            { "target": "#n", "name": "n", "type": "textbox", "value": "x" }
        ] });
        assert!(!input_call(FILL_FORM, &text_only));
        assert_eq!(decide(FILL_FORM, &text_only, Some(&hidden())), Decision::CarryOn);
        for pointer in ["checkbox", "radio"] {
            let fields = json!({ "fields": [
                { "target": "#c", "name": "c", "type": pointer, "value": "true" }
            ] });
            assert!(input_call(FILL_FORM, &fields), "a {pointer} is set with a click");
            refusal(FILL_FORM, &fields);
        }
        let mixed = json!({ "fields": [
            { "target": "#n", "name": "n", "type": "textbox", "value": "x" },
            { "target": "#c", "name": "c", "type": "checkbox", "value": "true" }
        ] });
        assert!(input_call(FILL_FORM, &mixed), "one pointer field makes the call pointer input");

        assert!(!input_call("browser_select_option", &json!({})));
        assert_eq!(decide("browser_select_option", &json!({}), Some(&hidden())), Decision::CarryOn,);

        // Waits are diagnosed after their own timeout, never refused: a
        // condition already true returns before any frame is polled.
        for wait in [json!({ "text": "Ready" }), json!({ "expression": "window.done" })] {
            assert!(!watched(WAIT, &wait), "a wait is diagnosed, not refused");
            assert_eq!(decide(WAIT, &wait, Some(&hidden())), Decision::CarryOn);
        }

        for tool in
            ["browser_navigate", "browser_evaluate", "browser_tabs", "browser_take_screenshot"]
        {
            assert_eq!(decide(tool, &json!({}), Some(&hidden())), Decision::CarryOn);
            assert!(!watched(tool, &json!({})), "{tool} pays no probe");
        }
        assert_eq!(
            decide(RUN_CODE, &json!({}), Some(&hidden())),
            Decision::CarryOn,
            "arbitrary code is never pre-refused",
        );
        assert!(!watched(RUN_CODE, &json!({})));
        assert_eq!(decide(CLICK, &json!({}), None), Decision::CarryOn, "a failed probe carries on",);
    }

    /// The snapshot rides the read that still answers: a Note on a hidden
    /// page, nothing on a visible one.
    #[test]
    fn the_snapshot_carries_the_state_only_on_a_hidden_page() {
        let Decision::Note(note) = decide(SNAPSHOT, &json!({}), Some(&hidden())) else {
            panic!("a hidden page's snapshot carries the state");
        };
        assert!(note.contains("document.visibilityState"), "{note}");
        assert!(note.contains("browser_tabs"), "{note}");
        assert_eq!(decide(SNAPSHOT, &json!({}), Some(&visible())), Decision::CarryOn);
        assert!(watched(SNAPSHOT, &json!({})), "the snapshot asks first");
    }

    /// **Every tool in `specs.rs` is classified.** The surface is read from
    /// the file itself rather than mirrored here, so a tool added there fails
    /// this test until it is classified; the count is the coverage assertion
    /// that the parse ran over the whole file.
    #[test]
    fn every_tool_on_the_surface_is_classified() {
        const SPECS: &str =
            include_str!("../../../../crates/forge-workspace/src/mcp/browser/specs.rs");
        const NON_INPUT: &[&str] = &[
            "browser_close",
            "browser_resize",
            "browser_console_messages",
            "browser_handle_dialog",
            "browser_emulate_media",
            "browser_evaluate",
            "browser_file_upload",
            "browser_find",
            "browser_press_key",
            "browser_type",
            "browser_navigate",
            "browser_navigate_back",
            "browser_network_requests",
            "browser_network_request",
            "browser_run_code_unsafe",
            "browser_take_screenshot",
            "browser_snapshot",
            "browser_select_option",
            "browser_tabs",
            "browser_wait_for",
            "browser_form_state",
            "browser_hand_off",
            "browser_profile_delete",
        ];
        const ARGUMENT_SHAPED: &[&str] = &[CLICK, FILL_FORM];

        let mut surface = Vec::new();
        let mut rest = SPECS;
        while let Some(at) = rest.find("name: \"browser_") {
            let tail = &rest[at + "name: \"".len()..];
            let Some(end) = tail.find('"') else { break };
            surface.push(&tail[..end]);
            rest = &tail[end..];
        }
        assert_eq!(
            surface.len(),
            29,
            "**coverage**: the parse must read every tool in specs.rs, not a prefix - classify \
             the new one and update this count: {surface:?}",
        );
        assert!(
            surface.contains(&"browser_click") && surface.contains(&"browser_profile_delete"),
            "**control**: the parse saw the first and last names: {surface:?}",
        );
        let mut unique = surface.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), surface.len(), "no name is read twice: {surface:?}");

        for tool in &surface {
            assert!(
                INPUT_TOOLS.contains(tool)
                    || ARGUMENT_SHAPED.contains(tool)
                    || NON_INPUT.contains(tool),
                "{tool} is on the surface and classified nowhere",
            );
        }
        for tool in INPUT_TOOLS.iter().chain(ARGUMENT_SHAPED).chain(NON_INPUT) {
            assert!(surface.contains(tool), "{tool} is classified but not on the surface");
        }
        for input in INPUT_TOOLS {
            assert!(!NON_INPUT.contains(input), "{input} is classified both ways");
            assert!(!ARGUMENT_SHAPED.contains(input), "{input} is classified both ways");
        }
        for shaped in ARGUMENT_SHAPED {
            assert!(!NON_INPUT.contains(shaped), "{shaped} is classified both ways");
        }
    }

    /// The refusal names the tab, the state and the remedy the driver really
    /// has - `browser_tabs select` is what runs `page.bringToFront`, under the
    /// same profile - and says the host will not raise it itself.
    #[test]
    fn a_hidden_pages_refusal_names_the_tab_and_the_remedy() {
        let why = refusal(CLICK, &json!({ "target": "#b" }));
        assert!(why.starts_with(CLICK), "the call is named: {why}");
        assert!(why.contains("\"Sign in\""), "the tab is named by title: {why}");
        assert!(why.contains("https://example.com/login"), "and by URL: {why}");
        assert!(why.contains("document.visibilityState"), "the state is named: {why}");
        assert!(why.contains("stability wait"), "the mechanism is named: {why}");
        assert!(why.contains("action \"list\""), "the list call is named: {why}");
        assert!(why.contains("action \"select\""), "the select call is named: {why}");
        assert!(why.contains("`profile`"), "the profile is named: {why}");
        assert!(why.contains("bringToFront"), "and what it runs: {why}");
    }

    /// A failure is re-probed and the diagnosis appended where the state
    /// explains it: a pointer call on a page that went hidden, a wait or an
    /// element screenshot whose own wait died hidden, and arbitrary code's
    /// own timeout. The call's own reason is never dropped.
    #[test]
    fn a_failure_is_named_when_the_page_is_hidden_now() {
        let hidden_tab = hidden();
        let visible_tab = visible();
        let click_args = json!({ "target": "#b" });
        let click_bound = action_bound(CLICK, &click_args);
        let click = |state: Option<&PageState>| {
            failure_verdict(CLICK, &click_args, "some failure", click_bound, state)
        };
        let appended = click(Some(&hidden_tab)).expect("an input call's late failure is named");
        assert!(appended.starts_with("some failure"), "the call's own reason is kept: {appended}");
        assert!(
            appended.contains("document.visibilityState"),
            "and the hidden page is appended as the explanation: {appended}",
        );
        assert_eq!(click(Some(&visible_tab)), None, "a visible page keeps the call's own failure",);
        assert_eq!(click(None), None);

        let code = |why: &str, state: Option<&PageState>| {
            failure_verdict(RUN_CODE, &json!({}), why, action_bound(RUN_CODE, &json!({})), state)
        };
        let timeout = driver::did_not_answer(RUN_CODE, action_bound(RUN_CODE, &json!({})));
        assert!(
            code(&timeout, Some(&hidden_tab))
                .is_some_and(|why| why.starts_with(&timeout) && why.contains("bringToFront")),
            "a code timeout on a now-hidden page keeps the reason and adds the diagnosis",
        );
        assert_eq!(
            code(&timeout, Some(&visible_tab)),
            None,
            "a visible page's timeout stands alone",
        );
        assert_eq!(code("a code error of its own", Some(&hidden_tab)), None);

        let wait_args = json!({ "text": "Ready" });
        let wait_bound = action_bound(WAIT, &wait_args);
        let wait = |why: &str, state: Option<&PageState>| {
            failure_verdict(WAIT, &wait_args, why, wait_bound, state)
        };
        assert!(
            wait("Timeout 5000ms exceeded", Some(&hidden_tab))
                .is_some_and(|why| why.contains("browser_wait_for")),
            "a wait's own timeout on a now-hidden page is diagnosed by name",
        );
        assert_eq!(
            wait("Either text, textGone or time must be provided", Some(&hidden_tab)),
            None,
            "a wait's own argument error is not a visibility problem",
        );
        assert!(
            wait("TimeoutError: a plugin blew up", Some(&hidden_tab)).is_none(),
            "only Playwright's own 'Timeout <n>ms' sentence is a timeout, not any word starting \
             with it",
        );

        let shot_args = json!({ "target": "e1" });
        let shot_bound = action_bound(SCREENSHOT, &shot_args);
        let shot = |why: &str, state: Option<&PageState>| {
            failure_verdict(SCREENSHOT, &shot_args, why, shot_bound, state)
        };
        assert!(
            shot("Timeout 30000ms exceeded", Some(&hidden_tab)).is_some(),
            "an element screenshot's stability wait is diagnosed"
        );
        assert_eq!(shot("Timeout 30000ms exceeded", Some(&visible_tab)), None);
        assert_eq!(
            shot("no element matches \"e1\"", Some(&hidden_tab)),
            None,
            "a lookup failure of its own stands",
        );
    }

    /// **The wiring's two pure halves.** `pre_call` runs the watched gate and
    /// the decision; `needs_reprobe` says which failures are worth asking
    /// the page about. The driver round trips stay live-only.
    #[test]
    fn the_wiring_halves_are_pinned() {
        let hidden_tab = hidden();
        let visible_tab = visible();
        let click = json!({ "target": "#b" });
        assert!(pre_call(CLICK, &click, Some(&hidden_tab)).is_err());
        assert!(
            pre_call(SNAPSHOT, &json!({}), Some(&hidden_tab))
                .expect("not refused")
                .is_some_and(|note| note.contains("document.visibilityState")),
            "the snapshot's note is attached",
        );
        assert_eq!(pre_call(CLICK, &click, Some(&visible_tab)), Ok(None));
        assert_eq!(pre_call("browser_navigate", &json!({}), Some(&hidden_tab)), Ok(None));
        assert_eq!(
            pre_call("browser_press_key", &json!({ "key": "Home" }), Some(&hidden_tab)),
            Ok(None),
            "a key press runs; a hidden page drops it silently",
        );

        let bound = action_bound(CLICK, &click);
        assert!(needs_reprobe(CLICK, &click, "any failure", bound));
        assert!(!needs_reprobe("browser_navigate", &json!({}), "any failure", bound));
        assert!(
            !needs_reprobe(CLICK, &json!({ "target": "#b", "force": true }), "any failure", bound),
            "a forced click takes no stability wait to explain",
        );
        let code_bound = action_bound(RUN_CODE, &json!({}));
        let code_timeout = driver::did_not_answer(RUN_CODE, code_bound);
        assert!(needs_reprobe(RUN_CODE, &json!({}), &code_timeout, code_bound));
        assert!(!needs_reprobe(RUN_CODE, &json!({}), "a code error of its own", code_bound));

        let wait = json!({ "text": "Ready" });
        let wait_bound = action_bound(WAIT, &wait);
        assert!(
            needs_reprobe(WAIT, &wait, "Timeout 5000ms exceeded", wait_bound),
            "a wait's own timeout is worth re-probing",
        );
        assert!(
            !needs_reprobe(
                WAIT,
                &wait,
                "Either text, textGone or time must be provided",
                wait_bound
            ),
            "a wait's argument error is not",
        );

        let shot = json!({ "target": "e1" });
        let shot_bound = action_bound(SCREENSHOT, &shot);
        assert!(needs_reprobe(SCREENSHOT, &shot, "Timeout 30000ms exceeded", shot_bound));
        assert!(!needs_reprobe(SCREENSHOT, &json!({}), "any failure", shot_bound));
    }

    /// **A malformed call is its own argument error, even on a hidden page.**
    /// The routing runs before the gate, so the driver's schema fault is not
    /// rewritten as a visibility refusal.
    #[test]
    fn a_malformed_call_keeps_its_argument_error() {
        let malformed = json!({ "force": true });
        assert!(
            routing_refusal(CLICK, &malformed).is_some_and(|why| why.contains("`target`")),
            "a forced click with no target is refused for the target",
        );
        assert!(routing_refusal(CLICK, &json!({ "target": "#b" })).is_none());
        assert!(
            routing_refusal("browser_navigate", &json!({ "url": "data:text/html,x" })).is_none()
        );
    }

    /// The probe fits inside the driver's own bound: the calls that re-probe
    /// give up a probe's worth of their action, and everything else keeps it.
    #[test]
    fn a_reprobed_call_leaves_the_probe_its_room() {
        let click = json!({ "target": "#b" });
        assert_eq!(action_bound(CLICK, &click), driver::CALL_TIMEOUT - PROBE_TIMEOUT,);
        assert_eq!(
            action_bound(CLICK, &json!({ "target": "#b", "force": true })),
            driver::CALL_TIMEOUT,
            "a forced click is not re-probed",
        );
        assert_eq!(action_bound(RUN_CODE, &json!({})), driver::CALL_TIMEOUT - PROBE_TIMEOUT);
        assert_eq!(
            action_bound(WAIT, &json!({ "text": "Ready" })),
            driver::CALL_TIMEOUT - PROBE_TIMEOUT,
        );
        assert_eq!(
            action_bound(SCREENSHOT, &json!({ "target": "e1" })),
            driver::CALL_TIMEOUT - PROBE_TIMEOUT,
        );
        assert_eq!(action_bound("browser_navigate", &json!({})), driver::CALL_TIMEOUT);
        assert_eq!(
            action_bound("browser_press_key", &json!({ "key": "Home" })),
            driver::CALL_TIMEOUT,
        );
    }

    /// One enormous name is clipped on a character boundary, so the remedy
    /// stays readable; a short one passes through.
    #[test]
    fn a_long_tab_name_is_clipped() {
        let long = PageState { hidden: true, url: "u".repeat(500), title: "t".repeat(500) };
        let name = tab_name(&long);
        assert!(name.chars().count() < 500, "{name}");
        assert!(name.contains("..."), "{name}");
        assert_eq!(tab_name(&hidden()), "\"Sign in\" (https://example.com/login)");

        let unicode = PageState { hidden: true, url: String::new(), title: "é".repeat(500) };
        assert!(tab_name(&unicode).chars().count() <= NAME_LIMIT + 3 + 2);
    }
}
