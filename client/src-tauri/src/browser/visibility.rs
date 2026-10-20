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
//! **A network failure names the address it was given, not the host that
//! failed**: a redirect can end the navigation elsewhere, and where the
//! browser is now is the one fact the reason cannot carry, so it is appended.
//!
//! **Desktop only.** The remedy is `page.bringToFront`, and it cannot make a
//! phone's WebView visible - the phone also keeps that WebView in an
//! invisible holder while no takeover is up, where this would fire on the
//! normal flow and the remedy could loop. The phone keeps the behaviour it
//! had.
//!
//! **The probe costs a driver round trip per attempt, and upstream's own
//! settle.** The vendored driver waits its `timeouts.settle` (500 ms) after
//! every tool call it runs through its own `waitForCompletion`,
//! `browser_evaluate` included, so a watched call on a visible page pays that
//! once before it runs, and a failure's probe pays it per attempt - up to
//! [`PROBE_ATTEMPTS`] of them. A CDP read of our own would skip the settle but
//! cannot address the driver's CURRENT tab when the browser holds several -
//! probing the wrong page would refuse a visible tab or drive a hidden one,
//! which is worse than the settle.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::driver::{self, Driver, ReplyPart};
use super::profiles::{routed_call, routing_refusal};

/// The longest one probe attempt is given. A live page answers an evaluate in
/// milliseconds; a probe that cannot answer inside this is no evidence about
/// the page, and a bound of its own keeps it off the call's.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How many attempts a failure's probe gets. **The first evaluate after a
/// failed navigation dies on the context that navigation destroyed**: the
/// shipped driver answered "Execution context was destroyed" on 8 of 8
/// measured runs at a refused address, and the next call read the page on 8
/// of 8. Each attempt is a driver round trip of its own, so the retry is also
/// the wait the error page's commit needs.
const PROBE_ATTEMPTS: u32 = 2;

/// The probe: one evaluate, the read a hidden page still answers.
const PROBE: &str = "() => ({ visibility: document.visibilityState, url: document.location.href, \
                     title: document.title })";

/// The tools that navigate as their whole job.
const NAVIGATING: &[&str] = &["browser_navigate", "browser_navigate_back", "browser_tabs"];

/// The prefixes Playwright puts on a navigation a snippet ran itself - `goto`,
/// `goBack`, `goForward` and `reload` - so a script that goes to a page is
/// admitted and one that only throws or evaluates is not.
const NAVIGATION_PREFIXES: &[&str] = &[
    "page.goto: net::ERR_",
    "page.goBack: net::ERR_",
    "page.goForward: net::ERR_",
    "page.reload: net::ERR_",
];

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

/// The bound a call's action gets. A call re-probed by its shape reserves
/// what its failure's probe can spend (PROBE_ATTEMPTS attempts), so the whole
/// call still fits inside the driver's bound. **A network failure is only
/// known after the fact**, so on any other tool its probe runs past this
/// bound by that much - slack in the wedge-breaker, not in the action.
pub(super) fn action_bound(tool: &str, args: &Value) -> Duration {
    if reprobed(tool, args) {
        driver::CALL_TIMEOUT.saturating_sub(PROBE_TIMEOUT * PROBE_ATTEMPTS)
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

/// The probe after a failure: bounded attempts, because the first one races
/// the error page's commit.
async fn probe_after_failure(driver: &Driver) -> Option<PageState> {
    for _ in 0..PROBE_ATTEMPTS {
        if let Some(state) = probe(driver).await {
            return Some(state);
        }
    }
    None
}

/// A call failed: ask the page, and name it when that explains the failure -
/// the host a network failure is on, or a page that is hidden now.
async fn after_failure(
    driver: &Driver,
    tool: &str,
    args: &Value,
    why: String,
    bound: Duration,
) -> String {
    if !failure_probe_needed(tool, args, &why, bound) {
        return why;
    }
    let state = probe_after_failure(driver).await;
    if note_dropped(tool, &why, state.as_ref()) {
        // **A dropped note is logged** - the note is forge's own feature, and
        // without it the reason reads as if the address it names is the host
        // that failed.
        let address = asked_address(args, &why);
        tauri_plugin_log::log::warn!(
            "the page could not be read after a network failure (event_name \
             browser_failure_probe_unreadable, tool {tool}, address {address}): the reason is \
             reported without the page it ended on"
        );
    }
    failure_verdict_for(tool, args, &why, bound, state.as_ref()).unwrap_or(why)
}

/// Whether a failure asks the page at all: a navigation's network failure is
/// named by the page, and a re-probeable call asks about its state.
fn failure_probe_needed(tool: &str, args: &Value, why: &str, bound: Duration) -> bool {
    net_failure(tool, why) || needs_reprobe(tool, args, why, bound)
}

/// Whether the note a network failure wanted was lost. **A page that was read
/// but had nothing to name is not a loss**: an untitled error page is
/// suppressed on purpose.
fn note_dropped(tool: &str, why: &str, state: Option<&PageState>) -> bool {
    net_failure(tool, why) && state.is_none()
}

/// The address a failed call asked for: its own `url` when the tool takes
/// one, else the one the driver's reason names (`... at https://...`), so a
/// snippet's navigation is logged with its address too.
fn asked_address<'a>(args: &'a Value, why: &'a str) -> &'a str {
    if let Some(url) = args.get("url").and_then(Value::as_str) {
        return url;
    }
    why.split_once(" at ")
        .and_then(|(_, address)| address.split_whitespace().next())
        .filter(|address| address.starts_with("http"))
        .unwrap_or("no address")
}

/// The verdict for a failed call, from the page the probe found. **The two
/// are told apart by the failure's shape, and the network note wins**: a
/// snippet can fail with a navigation's network error or with its own
/// timeout, and only the first is a fact about where the navigation ended -
/// a timeout still reaches the hidden-page diagnosis below.
fn failure_verdict_for(
    tool: &str,
    args: &Value,
    why: &str,
    bound: Duration,
    state: Option<&PageState>,
) -> Option<String> {
    if net_failure(tool, why) {
        return net_verdict(tool, why, state);
    }
    failure_verdict(tool, args, why, bound, state)
}

/// Whether the reason is the driver reporting a navigation's own network
/// failure: `net::ERR_x` and the address it was given (`at https://...`), on
/// a tool that navigates - or on a snippet whose failure names the page call
/// that navigated. **Both the tool and the shape decide**, so a script that
/// only throws a string carrying an address is not a page fact.
fn net_failure(tool: &str, why: &str) -> bool {
    let shaped = why.contains("net::ERR_") && why.contains(" at http");
    if !shaped {
        return false;
    }
    if NAVIGATING.contains(&tool) {
        return true;
    }
    tool == RUN_CODE && NAVIGATION_PREFIXES.iter().any(|prefix| why.contains(prefix))
}

/// The network half of a failure's verdict: the call's own reason kept, with
/// where the browser is now appended. **The guard repeats what the caller
/// established** so the note cannot be attached to a tool's own failure.
fn net_verdict(tool: &str, why: &str, state: Option<&PageState>) -> Option<String> {
    if !net_failure(tool, why) {
        return None;
    }
    let note = unreachable_note(state?)?;
    Some(format!("{why}\n\n{note}"))
}

/// Where the browser is now, as a failure's note. A Chromium error page
/// carries the host it failed on as its title; a page with nothing to name
/// adds nothing.
fn unreachable_note(state: &PageState) -> Option<String> {
    let title = state.title.trim();
    if state.url.starts_with("chrome-error://") {
        return (!title.is_empty()).then(|| {
            format!("The browser is on its own error page for \"{}\" now.", clipped(title))
        });
    }
    if title.is_empty() && state.url.is_empty() {
        return None;
    }
    Some(format!("The browser is on {} now.", tab_name(state)))
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

    /// **A refused navigation names the host the browser actually stopped at.**
    /// A Greenhouse job URL answered 302 to yieldmo.com, whose address this
    /// machine's DNS sinks to `0.0.0.0`; the call's own reason named the
    /// Greenhouse address, and the session chased a host that had answered.
    /// The page the driver is on is the fact the reason cannot carry.
    #[test]
    fn a_network_failure_names_the_page_the_browser_stopped_on() {
        let navigate = "browser_navigate";
        let refused = "Error: browserBackend.callTool: net::ERR_CONNECTION_REFUSED at \
                       https://job-boards.greenhouse.io/yieldmo/jobs/8243707";
        assert!(net_failure(navigate, refused), "the driver's navigation error is recognised");
        assert!(!net_failure(navigate, "Timeout 5000ms exceeded"), "a tool's timeout is not one");
        assert!(!net_failure(navigate, "no element matches \"e1\""), "nor a selector miss");
        assert!(
            !net_failure(RUN_CODE, "Error: net::ERR_FAKE at https://x.test/"),
            "**a script's throw carrying an address is not a navigation**: no page call is \
             named in it",
        );
        assert!(
            net_failure(
                RUN_CODE,
                "Error: page.goto: net::ERR_CONNECTION_REFUSED at https://x.test/"
            ),
            "a snippet that went to a page carries the same failure",
        );
        assert!(
            net_failure(
                RUN_CODE,
                "Error: page.reload: net::ERR_NAME_NOT_RESOLVED at https://x.test/",
            ),
            "and so does one that reloaded",
        );
        assert!(
            net_failure(
                RUN_CODE,
                "Error: page.goBack: net::ERR_CONNECTION_REFUSED at https://x.test/",
            ),
            "and one that went back",
        );
        assert!(
            net_failure(
                RUN_CODE,
                "Error: page.goForward: net::ERR_CONNECTION_REFUSED at https://x.test/",
            ),
            "and one that went forward",
        );
        assert!(
            !net_failure(RUN_CODE, "Error: page.evaluate: net::ERR_FAKE at https://x.test/",),
            "**a page call that does not navigate is not admitted**: the prefix is the page \
             call's, not a bare `page.`",
        );

        let error_page = PageState {
            hidden: false,
            url: "chrome-error://chromewebdata/".to_owned(),
            title: "yieldmo.com".to_owned(),
        };
        let verdict =
            net_verdict(navigate, refused, Some(&error_page)).expect("a network failure is named");
        assert!(verdict.starts_with(refused), "the call's own reason is kept: {verdict}");
        assert!(
            verdict.contains("its own error page for \"yieldmo.com\""),
            "the host it stopped at is named, as the error page's own: {verdict}",
        );

        // **The note says where the browser is, not how it got there.** A
        // navigation that aborts leaves the tab where it was, and a failed
        // resolution names the address that was asked for; neither is a
        // redirect, so the note claims neither.
        let previous = PageState {
            hidden: false,
            url: "https://example.com/previous".to_owned(),
            title: "Previous".to_owned(),
        };
        let aborted = net_verdict(
            navigate,
            "page.goto: net::ERR_ABORTED at https://x.test/",
            Some(&previous),
        )
        .expect("an aborted navigation still says where the browser is");
        assert!(
            aborted.contains("\"Previous\" (https://example.com/previous)"),
            "the page it is on now is named: {aborted}",
        );
        assert!(!aborted.contains("error page"), "and it is not called an error page: {aborted}");

        // **A snippet that navigates is named the same way**, and one that
        // only throws is not named at all.
        let snippet = "Error: page.goto: net::ERR_CONNECTION_REFUSED at https://x.test/";
        let from_snippet = failure_verdict_for(
            RUN_CODE,
            &json!({}),
            snippet,
            driver::CALL_TIMEOUT,
            Some(&error_page),
        )
        .expect("a snippet's refused navigation is named");
        assert!(from_snippet.contains("yieldmo.com"), "{from_snippet}");
        assert_eq!(
            failure_verdict_for(
                RUN_CODE,
                &json!({}),
                "Error: net::ERR_FAKE at https://x.test/",
                driver::CALL_TIMEOUT,
                Some(&error_page),
            ),
            None,
            "a script's own throw is left as it is",
        );

        // **An error page with no title names no host**, so there is no note
        // to add; the session is left with the reason alone.
        let untitled = PageState {
            hidden: false,
            url: "chrome-error://chromewebdata/".to_owned(),
            title: String::new(),
        };
        assert_eq!(net_verdict(navigate, refused, Some(&untitled)), None);

        // One enormous host name is clipped like every other tab name, so it
        // cannot bury the reason.
        let enormous = PageState {
            hidden: false,
            url: "chrome-error://chromewebdata/".to_owned(),
            title: "h".repeat(500),
        };
        let clipped_note = net_verdict(navigate, refused, Some(&enormous)).expect("named");
        assert!(clipped_note.contains("..."), "the name is clipped: {clipped_note}");
        assert!(clipped_note.chars().count() < 500, "{clipped_note}");

        assert_eq!(
            net_verdict(navigate, refused, None),
            None,
            "a page that cannot be read is no evidence and adds nothing",
        );
        assert_eq!(
            net_verdict(navigate, "no element matches \"e1\"", Some(&error_page)),
            None,
            "a tool's own failure is not given a page",
        );
    }

    /// **The route to the page is the failure's own, not the tool's.** A
    /// navigating tool's network failure asks the page - `browser_navigate` is
    /// in no re-probe set - while that same tool's ordinary failure does not,
    /// and a script's throw that carries an address is not a network failure.
    #[test]
    fn a_network_failure_asks_the_page_on_a_navigating_tool() {
        let navigate = json!({ "url": "https://nope.invalid/" });
        let bound = action_bound("browser_navigate", &navigate);
        let refused =
            "browserBackend.callTool: net::ERR_NAME_NOT_RESOLVED at https://nope.invalid/";
        assert!(failure_probe_needed("browser_navigate", &navigate, refused, bound));
        assert!(
            !failure_probe_needed(
                "browser_navigate",
                &navigate,
                "no element matches \"e1\"",
                bound
            ),
            "the same tool's ordinary failure is not worth a page read",
        );
        assert!(
            !failure_probe_needed(RUN_CODE, &json!({}), refused, bound),
            "a script that throws the driver's words asks for no page",
        );
        assert!(!failure_probe_needed(
            "browser_press_key",
            &json!({ "key": "Home" }),
            "a key was refused",
            bound,
        ));
    }

    /// **The two verdicts are told apart by the failure's shape, and the
    /// network note wins.** A navigation's network failure is named by the
    /// page even when that page is hidden - on a navigating tool or on a
    /// snippet; the hidden-page diagnosis is for the failures it explains.
    #[test]
    fn a_network_note_and_a_hidden_diagnosis_do_not_compete() {
        let refused = "browserBackend.callTool: net::ERR_CONNECTION_REFUSED at https://x.test/";
        let hidden_error = PageState {
            hidden: true,
            url: "chrome-error://chromewebdata/".to_owned(),
            title: "yieldmo.com".to_owned(),
        };
        let network = failure_verdict_for(
            "browser_navigate",
            &json!({}),
            refused,
            driver::CALL_TIMEOUT,
            Some(&hidden_error),
        )
        .expect("a network failure is named");
        assert!(network.contains("yieldmo.com"), "{network}");
        assert!(
            !network.contains("document.visibilityState"),
            "the page's name is the verdict, hidden page included: {network}",
        );

        // **A snippet can carry either shape**, and the network one wins:
        // its navigation failure is a page fact, its own timeout is not.
        let snippet = "Error: page.goto: net::ERR_CONNECTION_REFUSED at https://x.test/";
        let from_snippet = failure_verdict_for(
            RUN_CODE,
            &json!({}),
            snippet,
            driver::CALL_TIMEOUT,
            Some(&hidden_error),
        )
        .expect("a snippet's refused navigation is named");
        assert!(from_snippet.contains("yieldmo.com"), "{from_snippet}");
        assert!(
            !from_snippet.contains("document.visibilityState"),
            "the page's name is the verdict, hidden page included: {from_snippet}",
        );

        let diagnosed = failure_verdict_for(
            CLICK,
            &json!({ "target": "#b" }),
            "some failure",
            driver::CALL_TIMEOUT,
            Some(&hidden()),
        )
        .expect("a hidden page is diagnosed on the calls it explains");
        assert!(diagnosed.contains("document.visibilityState"), "{diagnosed}");
    }

    /// **The address a failure is logged with** is the call's own, else the
    /// one the driver's reason names - a snippet has no `url` argument.
    #[test]
    fn a_failure_names_the_address_it_asked_for() {
        let asked = json!({ "url": "https://asked.test/" });
        assert_eq!(
            asked_address(&asked, "page.goto: net::ERR_X at https://else.test/"),
            "https://asked.test/"
        );
        assert_eq!(
            asked_address(&json!({}), "Error: page.goto: net::ERR_X at https://else.test/"),
            "https://else.test/",
        );
        assert_eq!(asked_address(&json!({}), "no address in this reason"), "no address");
    }

    /// **A lost note is a loss only when the page could not be read.** A page
    /// that was read and had nothing to name is the deliberate suppression.
    #[test]
    fn a_dropped_note_is_the_unreadable_page_and_nothing_else() {
        let refused = "browserBackend.callTool: net::ERR_CONNECTION_REFUSED at https://x.test/";
        assert!(note_dropped("browser_navigate", refused, None));
        let untitled = PageState {
            hidden: false,
            url: "chrome-error://chromewebdata/".to_owned(),
            title: String::new(),
        };
        assert!(
            !note_dropped("browser_navigate", refused, Some(&untitled)),
            "a read page with no host is suppressed on purpose, not dropped",
        );
        assert!(!note_dropped("browser_navigate", "no element matches \"e1\"", None));
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

    /// The whole call fits inside the driver's own bound for the calls
    /// re-probed by their shape: those give up what every attempt of their
    /// failure's probe can spend. A network failure is only known after the
    /// fact, so `browser_navigate` - which is in no re-probe set - keeps its
    /// whole bound and its probe runs past it.
    #[test]
    fn a_reprobed_call_leaves_the_probe_its_room() {
        let probe_room = PROBE_TIMEOUT * PROBE_ATTEMPTS;
        let click = json!({ "target": "#b" });
        assert_eq!(action_bound(CLICK, &click), driver::CALL_TIMEOUT - probe_room);
        assert_eq!(
            action_bound(CLICK, &json!({ "target": "#b", "force": true })),
            driver::CALL_TIMEOUT,
            "a forced click is not re-probed",
        );
        assert_eq!(action_bound(RUN_CODE, &json!({})), driver::CALL_TIMEOUT - probe_room);
        assert_eq!(
            action_bound(WAIT, &json!({ "text": "Ready" })),
            driver::CALL_TIMEOUT - probe_room,
        );
        assert_eq!(
            action_bound(SCREENSHOT, &json!({ "target": "e1" })),
            driver::CALL_TIMEOUT - probe_room,
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
