//! The browser calls the host answers itself, as snippets over the same
//! driver.
//!
//! Two tools upstream does not have (`browser_click_and_capture`,
//! `browser_form_state`) and two arguments it does not take (`force` on a
//! click, `expression` on a wait). Each is a question a call cannot compose
//! out of the 25 - a click whose meaning is the request it fired, a form's
//! state where the committed value lives outside the input, a click that
//! means "it is there, click it anyway" - so each becomes a Playwright
//! snippet run through the driver's own `browser_run_code_unsafe`.
//!
//! **Refs are the driver's own `aria-ref` engine**, which is built into the
//! pinned playwright-core and is what the driver's own click resolves a
//! snapshot's target with: a snippet can therefore drive the same element a
//! `browser_snapshot` handed the model, which is what makes these additions
//! usable in a flow rather than only from a fresh navigation.
//!
//! A call that arrives with an argument upstream does not declare is a call
//! the driver would refuse, so the added ones are **stripped** on the way
//! through: what reaches upstream is its own shape, always.

use serde_json::Value;

/// What to do with one call: hand it to the driver as upstream's tool, or
/// answer it with a snippet of this host's own.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Routed {
    /// Upstream's own tool, under its own name. Only the arguments may have
    /// had an added one taken off.
    Upstream { args: Value },
    /// A Playwright snippet, run through the driver's `browser_run_code_unsafe`.
    Snippet(String),
}

/// Every advertised tool the pinned driver does not publish, with the arm
/// that answers it instead:
///
/// - `browser_click_and_capture` and `browser_form_state` are `route`'s
///   snippets, run below as the driver's own `browser_run_code_unsafe`.
/// - `browser_challenge_state` is composed in `service::driver_call` as an
///   evaluate.
/// - `browser_hand_off` parks on the person at the MCP tool layer
///   (`forge-workspace`), so it never reaches this crate.
/// - `browser_record_start` and `browser_record_stop` are arms of
///   `BrowserService::call`, answered by the host's own encoder.
///
/// The live parity check holds the driver against this list, so a tool whose
/// arm is deleted and whose name is left here is the one gap it cannot see;
/// each name's own test drives it through the same dispatch, so the arm and
/// the name fail together.
pub const HOST_ANSWERED: [&str; 6] = [
    "browser_challenge_state",
    "browser_click_and_capture",
    "browser_form_state",
    "browser_hand_off",
    "browser_record_start",
    "browser_record_stop",
];

/// Whether `name` is one of the tools forge answers because the pinned driver
/// does not publish it.
pub fn answers(name: &str) -> bool {
    HOST_ANSWERED.contains(&name)
}

/// Every advertised tool forge answers because the pinned driver does not
/// publish it, for the live parity check. A host-shaped tool the driver does
/// publish (`browser_tabs`, `browser_close`, `browser_resize`,
/// `browser_file_upload`) is deliberately absent.
pub fn host_answered() -> &'static [&'static str] {
    &HOST_ANSWERED
}

/// Route one call.
///
/// `Err` for a call this host cannot make sense of - a `force` click with no
/// target, a wait with both a time and an expression - so the session reads
/// why rather than the driver reading a shape it cannot take.
pub(crate) fn route(tool: &str, args: Value) -> Result<Routed, String> {
    let upstream = |args: Value| Ok(Routed::Upstream { args });
    match tool {
        "browser_click" => {
            if args.get("force").and_then(Value::as_bool) != Some(true) {
                // Absent or false: upstream's own click, with the added
                // argument taken off so its schema accepts the call.
                let mut stripped = args;
                if let Some(fields) = stripped.as_object_mut() {
                    fields.remove("force");
                }
                return upstream(stripped);
            }
            let target = target_of(&args)?;
            // Playwright's own options: `force` skips the actionability
            // gate, a double click is its own method, and a button and
            // modifiers ride the same call.
            let mut options = vec!["force: true".to_owned()];
            if let Some(button) = args.get("button").and_then(Value::as_str) {
                match button {
                    "left" | "right" | "middle" => options.push(format!("button: \"{button}\"")),
                    other => return Err(format!("`button` is not one a click takes: {other}")),
                }
            }
            if let Some(modifiers) = args.get("modifiers").and_then(Value::as_array) {
                let mut names = Vec::new();
                for modifier in modifiers {
                    let Some(name) = modifier.as_str() else {
                        return Err("every modifier is a name".to_owned());
                    };
                    match name {
                        // `ControlOrMeta` is the model's spelling; Playwright
                        // takes the platform's own key.
                        "ControlOrMeta" => names.push(js_string(if cfg!(target_os = "macos") {
                            "Meta"
                        } else {
                            "Control"
                        })),
                        "Alt" | "Control" | "Meta" | "Shift" => names.push(js_string(name)),
                        other => {
                            return Err(format!("`{other}` is not a modifier a click takes"));
                        }
                    }
                }
                options.push(format!("modifiers: [{}]", names.join(", ")));
            }
            let method = if args.get("doubleClick").and_then(Value::as_bool) == Some(true) {
                "dblclick"
            } else {
                "click"
            };
            Ok(Routed::Snippet(format!(
                "async (page) => {{ await {target}.{method}({{ {} }}); return 'clicked'; }}",
                options.join(", "),
            )))
        }
        "browser_wait_for" => {
            let Some(expression) = args.get("expression").and_then(Value::as_str) else {
                let mut stripped = args;
                if let Some(fields) = stripped.as_object_mut() {
                    fields.remove("expression");
                }
                return upstream(stripped);
            };
            if args.get("text").is_some()
                || args.get("textGone").is_some()
                || args.get("time").is_some()
            {
                return Err(
                    "`expression` waits on its own: pass it without `text`, `textGone` or `time`"
                        .to_owned(),
                );
            }
            Ok(Routed::Snippet(format!(
                "async (page) => {{ await page.waitForFunction({}); return 'the condition holds'; }}",
                js_string(expression),
            )))
        }
        "browser_click_and_capture" => {
            let target = target_of(&args)?;
            let settle = args.get("settleMs").and_then(Value::as_f64).unwrap_or(1500.0);
            if !(0.0..=30_000.0).contains(&settle) {
                return Err(
                    "`settleMs` is milliseconds to keep listening, at most 30000".to_owned()
                );
            }
            Ok(Routed::Snippet(format!(
                "async (page) => {{\
                 const seen = [];\
                 const onResponse = async (response) => {{\
                 let body = '';\
                 try {{ body = (await response.text()).slice(0, 4000); }} catch (why) {{ body = '<unreadable: ' + why.message + '>'; }}\
                 seen.push({{ url: response.url(), status: response.status(), body }});\
                 }};\
                 page.on('response', onResponse);\
                 await {target}.click({{}});\
                 await page.waitForTimeout({settle});\
                 page.off('response', onResponse);\
                 return JSON.stringify(seen);\
                 }}",
            )))
        }
        "browser_form_state" => {
            // A target is a snapshot ref or a selector, the same as every
            // other added tool's; with no target, every form on the page.
            let scope = match args.get("target") {
                None | Some(Value::Null) => "page.locator('form')".to_owned(),
                Some(_) => target_of(&args)?,
            };
            // Read in the PAGE, one handle at a time: a locator is not a
            // node, so `querySelectorAll` belongs inside an evaluate where
            // the argument is the element itself.
            Ok(Routed::Snippet(format!(
                "async (page) => {{\
                 const handles = (await {scope}.elementHandles()).slice(0, 20);\
                 const roots = [];\
                 for (const handle of handles) {{\
                 const fields = await handle.evaluate((el) => {{\
                 const selector = 'input, select, textarea, [role=\"combobox\"], [role=\"listbox\"], [aria-invalid]';\
                 const read = (node) => ({{\
                 tag: node.tagName.toLowerCase(),\
                 name: node.getAttribute('name') || '',\
                 type: node.getAttribute('type') || '',\
                 value: 'value' in node ? node.value : null,\
                 checked: typeof node.checked === 'boolean' ? node.checked : null,\
                 invalid: node.getAttribute('aria-invalid') === 'true' || node.matches(':invalid') === true,\
                 text: (node.innerText || node.textContent || '').trim().slice(0, 200),\
                 }});\
                 const own = el.matches(selector) ? [el] : [];\
                 return [...own, ...el.querySelectorAll(selector)].map(read).slice(0, 100);\
                 }});\
                 const control = await handle.evaluate((el) => ({{\
                 text: (el.innerText || el.textContent || '').trim().slice(0, 200),\
                 invalid: el.getAttribute('aria-invalid') === 'true',\
                 }}));\
                 roots.push({{ ...control, fields }});\
                 }}\
                 return JSON.stringify({{ roots }});\
                 }}",
            )))
        }
        _ => upstream(args),
    }
}

/// The locator for a call's `target`: the driver's own ref engine for a
/// snapshot ref, and a plain selector for anything else a caller wrote.
fn target_of(args: &Value) -> Result<String, String> {
    let Some(target) = args.get("target").and_then(Value::as_str) else {
        return Err("this call needs a `target`: a snapshot ref or a selector".to_owned());
    };
    let selector = if is_ref(target) { format!("aria-ref={target}") } else { target.to_owned() };
    Ok(format!("page.locator({})", js_string(&selector)))
}

/// Whether a target is one of the driver's snapshot refs: `e7` while the page
/// is on its first document, `f2e7` once a navigation has relettered them with
/// the frame's sequence. The engine's refs carry a number, and so does the
/// frame sequence ahead of the `e`.
fn is_ref(target: &str) -> bool {
    let tail = match target.strip_prefix('f') {
        Some(rest) => match rest.find('e') {
            Some(at) if at > 0 && rest[..at].bytes().all(|b| b.is_ascii_digit()) => &rest[at..],
            _ => return false,
        },
        None => target,
    };
    match tail.strip_prefix('e') {
        Some(number) => !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// One JavaScript string literal, escaped.
fn js_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // A lone surrogate or a control character would end the literal
            // early in the page's own parse.
            ch if (ch as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Upstream's own calls pass through untouched, and the two added
    /// arguments are STRIPPED when they are not in use: an argument the
    /// driver's schema does not declare is a call it refuses.
    #[test]
    fn upstreams_calls_pass_through_with_the_added_arguments_taken_off() {
        assert_eq!(
            route("browser_navigate", json!({ "url": "https://example.com" })),
            Ok(Routed::Upstream { args: json!({ "url": "https://example.com" }) }),
        );
        assert_eq!(
            route("browser_click", json!({ "target": "e5" })),
            Ok(Routed::Upstream { args: json!({ "target": "e5" }) }),
            "no `force`, so upstream's own click - and the argument is not on the call",
        );
        assert_eq!(
            route("browser_click", json!({ "target": "e5", "force": false })),
            Ok(Routed::Upstream { args: json!({ "target": "e5" }) }),
            "`force: false` is upstream's click with the argument taken off",
        );
        assert_eq!(
            route("browser_wait_for", json!({ "text": "Ready" })),
            Ok(Routed::Upstream { args: json!({ "text": "Ready" }) }),
        );
        assert!(
            matches!(
                route("browser_wait_for", json!({ "text": "Ready", "expression": "true" })),
                Err(why) if why.contains("on its own"),
            ),
            "an expression and a text together is a call with two readings",
        );
    }

    /// A forced click is a snippet that clicks the SAME element a snapshot
    /// handed the model - the driver's own ref engine - with the
    /// actionability gate off.
    #[test]
    fn a_forced_click_is_a_snippet_over_the_drivers_own_refs() {
        let Routed::Snippet(code) =
            route("browser_click", json!({ "target": "e12", "force": true })).expect("routes")
        else {
            panic!("a forced click is a snippet");
        };
        assert!(code.contains("aria-ref=e12"), "{code}");
        assert!(code.contains("force: true"), "{code}");
        assert!(code.contains(".click("), "a click is Playwright's own click: {code}");

        let Routed::Snippet(double) = route(
            "browser_click",
            json!({ "target": "e12", "force": true, "doubleClick": true, "button": "right",
                     "modifiers": ["Shift", "ControlOrMeta"] }),
        )
        .expect("routes") else {
            panic!("a forced double click is a snippet");
        };
        assert!(double.contains(".dblclick("), "{double}");
        assert!(double.contains("button: \"right\""), "{double}");
        assert!(double.contains("modifiers: [\"Shift\""), "{double}");
        assert!(
            double.contains(if cfg!(target_os = "macos") { "\"Meta\"" } else { "\"Control\"" }),
            "ControlOrMeta is the platform's own key: {double}",
        );
    }

    /// A selector the caller wrote is passed as a selector; only a snapshot
    /// ref takes the ref engine.
    #[test]
    fn only_a_snapshot_ref_takes_the_ref_engine() {
        let Routed::Snippet(code) =
            route("browser_click", json!({ "target": "#submit", "force": true })).expect("routes")
        else {
            panic!("a forced click is a snippet");
        };
        assert!(code.contains("page.locator(\"#submit\")"), "{code}");
        assert!(!code.contains("aria-ref"), "{code}");

        // **Ref-shaped is not a ref**: the engine's refs carry a number after
        // the `e`, and the frame sequence ahead of it carries one too.
        for not_a_ref in ["e", "f1", "fe12", "f1xe3", "f1e7x"] {
            let Routed::Snippet(bare) =
                route("browser_click", json!({ "target": not_a_ref, "force": true }))
                    .expect("routes")
            else {
                panic!("a forced click is a snippet");
            };
            assert!(
                bare.contains(&format!("page.locator(\"{not_a_ref}\")")),
                "{not_a_ref} is a selector the caller wrote: {bare}",
            );
            assert!(
                !bare.contains("aria-ref"),
                "{not_a_ref} must not reach the ref engine: {bare}",
            );
        }
    }

    /// **A navigation re-letters the driver's refs**: the second document's
    /// snapshot spells the same element `f1e7`, so that spelling has to reach
    /// the ref engine too - `targetLocators` in the vendored driver takes a
    /// target as a ref on `/^(f\d+)?e\d+$/` and otherwise as a selector.
    #[test]
    fn a_relettered_ref_still_takes_the_ref_engine() {
        let Routed::Snippet(code) =
            route("browser_click", json!({ "target": "f1e7", "force": true })).expect("routes")
        else {
            panic!("a forced click is a snippet");
        };
        assert!(code.contains("aria-ref=f1e7"), "a relettered ref takes the ref engine: {code}");

        let Routed::Snippet(capture) =
            route("browser_click_and_capture", json!({ "target": "f2e3" })).expect("routes")
        else {
            panic!("click_and_capture is a snippet");
        };
        assert!(
            capture.contains("aria-ref=f2e3"),
            "a relettered ref reaches the engine here too: {capture}",
        );

        let Routed::Snippet(scoped) =
            route("browser_form_state", json!({ "target": "f1e7" })).expect("routes")
        else {
            panic!("form_state is a snippet");
        };
        assert!(
            scoped.contains("aria-ref=f1e7"),
            "and a form scoped to a relettered ref reads that element: {scoped}",
        );
    }

    /// The two beyond-upstream tools are snippets, and each carries what it
    /// was asked for.
    #[test]
    fn the_added_tools_are_snippets_of_their_own() {
        let Routed::Snippet(capture) =
            route("browser_click_and_capture", json!({ "target": "e3", "settleMs": 500 }))
                .expect("routes")
        else {
            panic!("click_and_capture is a snippet");
        };
        assert!(capture.contains("aria-ref=e3"), "{capture}");
        assert!(capture.contains("waitForTimeout(500)"), "{capture}");
        assert!(capture.contains("on('response'"), "{capture}");

        let Routed::Snippet(form) = route("browser_form_state", json!({})).expect("routes") else {
            panic!("form_state is a snippet");
        };
        assert!(form.contains("aria-invalid"), "{form}");
        assert!(
            form.contains("elementHandles") && form.contains("handle.evaluate"),
            "the read happens in the page, on the element itself: {form}",
        );
        assert!(form.contains("'value' in node"), "{form}");

        let Routed::Snippet(scoped) =
            route("browser_form_state", json!({ "target": "e7" })).expect("routes")
        else {
            panic!("form_state is a snippet");
        };
        assert!(scoped.contains("aria-ref=e7"), "{scoped}");

        // The schema says "a snapshot ref, or a unique element selector", so
        // a selector must reach the page as itself rather than through the
        // ref engine.
        let Routed::Snippet(selector) =
            route("browser_form_state", json!({ "target": "#signup" })).expect("routes")
        else {
            panic!("form_state is a snippet");
        };
        assert!(selector.contains("page.locator(\"#signup\")"), "{selector}");
        assert!(!selector.contains("aria-ref"), "{selector}");
    }

    /// The wait's expression is escaped into a JavaScript literal, so a quote
    /// or a newline in it cannot end the snippet early.
    #[test]
    fn an_expression_is_escaped_into_its_literal() {
        let Routed::Snippet(code) =
            route("browser_wait_for", json!({ "expression": "a\"b\nc" })).expect("routes")
        else {
            panic!("an expression wait is a snippet");
        };
        assert!(code.contains(r#""a\"b\nc""#), "{code}");
        assert!(code.contains("waitForFunction"), "{code}");
    }

    /// The host-answered set is what the parity check holds the driver
    /// against, so it names every advertised tool the driver does not
    /// publish and nothing else; the two this module answers are tied to
    /// their snippets so a name here cannot outlive its arm.
    #[test]
    fn the_host_answered_set_is_every_tool_forge_answers() {
        assert_eq!(
            host_answered(),
            &[
                "browser_challenge_state",
                "browser_click_and_capture",
                "browser_form_state",
                "browser_hand_off",
                "browser_record_start",
                "browser_record_stop",
            ],
        );
        for name in ["browser_click_and_capture", "browser_form_state"] {
            assert!(
                matches!(route(name, json!({ "target": "#x" })), Ok(Routed::Snippet(_))),
                "{name} is named host-answered, so it must route to a snippet",
            );
        }
        assert!(answers("browser_form_state"));
        assert!(!answers("browser_click"), "a tool the driver publishes is not host-answered");
    }
}
