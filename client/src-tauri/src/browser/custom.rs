//! The browser calls this host answers itself, as snippets over the same
//! driver.
//!
//! Three of them: two tools upstream does not have
//! (`browser_click_and_capture`, `browser_form_state`) and two arguments it
//! does not take (`force` on a click, `expression` on a wait). Each is a
//! question a call cannot compose out of the 25 - a click whose meaning is
//! the request it fired, a form's state where the committed value lives
//! outside the input, a click that means "it is there, click it anyway" - so
//! each becomes a Playwright snippet run through the driver's own
//! `browser_run_code_unsafe`.
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

use std::path::Path;

use serde_json::Value;

/// The snippet that saves a named context: its cookies go to `storage`, which
/// the driver reads back when the context is opened again, and the open tab
/// URLs come back one per line.
pub(super) fn save_session(storage: &Path) -> String {
    format!(
        "async (page) => {{ await page.context().storageState({{ path: {} }}); \
         return page.context().pages().map((p) => p.url()).join('\\n'); }}",
        js_string(&storage.to_string_lossy()),
    )
}

/// What to do with one call: hand it to the driver as upstream's tool, or
/// answer it with a snippet of this host's own.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Routed {
    Upstream { tool: String, args: Value },
    Snippet(String),
}

/// Route one call.
///
/// `Err` for a call this host cannot make sense of - a `force` click with no
/// target, a wait with both a time and an expression - so the session reads
/// why rather than the driver reading a shape it cannot take.
pub(super) fn route(tool: &str, args: &Value) -> Result<Routed, String> {
    let as_upstream = |args: Value| Ok(Routed::Upstream { tool: tool.to_owned(), args });
    match tool {
        "browser_click" => {
            if args.get("force").and_then(Value::as_bool) != Some(true) {
                // Absent or false: upstream's own click, with the added
                // argument taken off so its schema accepts the call.
                let mut stripped = args.clone();
                if let Some(fields) = stripped.as_object_mut() {
                    fields.remove("force");
                }
                return as_upstream(stripped);
            }
            let target = target_of(args)?;
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
                let mut stripped = args.clone();
                if let Some(fields) = stripped.as_object_mut() {
                    fields.remove("expression");
                }
                return as_upstream(stripped);
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
            let target = target_of(args)?;
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
            let scope = match args.get("target").and_then(Value::as_str) {
                Some(target) => {
                    format!("page.locator({})", js_string(&format!("aria-ref={target}")))
                }
                None => "page.locator('form')".to_owned(),
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
        _ => as_upstream(args.clone()),
    }
}

/// The locator for a call's `target`: the driver's own ref engine for a
/// snapshot ref, and a plain selector for anything else a caller wrote.
fn target_of(args: &Value) -> Result<String, String> {
    let Some(target) = args.get("target").and_then(Value::as_str) else {
        return Err("this call needs a `target`: a snapshot ref or a selector".to_owned());
    };
    let selector = if target.starts_with('e') && target[1..].chars().all(|c| c.is_ascii_digit()) {
        format!("aria-ref={target}")
    } else {
        target.to_owned()
    };
    Ok(format!("page.locator({})", js_string(&selector)))
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

    /// The save snippet writes the context's cookies to the file it is given -
    /// escaped, since the path lands inside a JavaScript literal - and hands
    /// its open tab URLs back one per line.
    #[test]
    fn the_save_snippet_names_its_storage_file_and_the_tab_urls() {
        let snippet = save_session(Path::new("/data/ctx/a\"b.json"));
        assert!(snippet.contains("storageState({ path: \"/data/ctx/a\\\"b.json\" })"), "{snippet}",);
        assert!(snippet.contains("pages()"), "{snippet}");
        assert!(snippet.contains("join('\\n')"), "{snippet}");
    }

    /// Upstream's own calls pass through untouched, and the two added
    /// arguments are STRIPPED when they are not in use: an argument the
    /// driver's schema does not declare is a call it refuses.
    #[test]
    fn upstreams_calls_pass_through_with_the_added_arguments_taken_off() {
        assert_eq!(
            route("browser_navigate", &json!({ "url": "https://example.com" })),
            Ok(Routed::Upstream {
                tool: "browser_navigate".to_owned(),
                args: json!({ "url": "https://example.com" }),
            }),
        );
        assert_eq!(
            route("browser_click", &json!({ "target": "e5" })),
            Ok(Routed::Upstream {
                tool: "browser_click".to_owned(),
                args: json!({ "target": "e5" }),
            }),
            "no `force`, so upstream's own click - and the argument is not on the call",
        );
        assert_eq!(
            route("browser_click", &json!({ "target": "e5", "force": false })),
            Ok(Routed::Upstream {
                tool: "browser_click".to_owned(),
                args: json!({ "target": "e5" }),
            }),
            "`force: false` is upstream's click with the argument taken off",
        );
        assert_eq!(
            route("browser_wait_for", &json!({ "text": "Ready" })),
            Ok(Routed::Upstream {
                tool: "browser_wait_for".to_owned(),
                args: json!({ "text": "Ready" }),
            }),
        );
        assert!(
            matches!(
                route("browser_wait_for", &json!({ "text": "Ready", "expression": "true" })),
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
            route("browser_click", &json!({ "target": "e12", "force": true })).expect("routes")
        else {
            panic!("a forced click is a snippet");
        };
        assert!(code.contains("aria-ref=e12"), "{code}");
        assert!(code.contains("force: true"), "{code}");
        assert!(code.contains(".click("), "a click is Playwright's own click: {code}");

        let Routed::Snippet(double) = route(
            "browser_click",
            &json!({ "target": "e12", "force": true, "doubleClick": true, "button": "right",
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

    /// A selector the caller wrote is passed as a selector; only a bare
    /// snapshot ref takes the ref engine.
    #[test]
    fn only_a_bare_ref_takes_the_ref_engine() {
        let Routed::Snippet(code) =
            route("browser_click", &json!({ "target": "#submit", "force": true })).expect("routes")
        else {
            panic!("a forced click is a snippet");
        };
        assert!(code.contains("page.locator(\"#submit\")"), "{code}");
        assert!(!code.contains("aria-ref"), "{code}");
    }

    /// The two beyond-upstream tools are snippets, and each carries what it
    /// was asked for.
    #[test]
    fn the_added_tools_are_snippets_of_their_own() {
        let Routed::Snippet(capture) =
            route("browser_click_and_capture", &json!({ "target": "e3", "settleMs": 500 }))
                .expect("routes")
        else {
            panic!("click_and_capture is a snippet");
        };
        assert!(capture.contains("aria-ref=e3"), "{capture}");
        assert!(capture.contains("waitForTimeout(500)"), "{capture}");
        assert!(capture.contains("on('response'"), "{capture}");

        let Routed::Snippet(form) = route("browser_form_state", &json!({})).expect("routes") else {
            panic!("form_state is a snippet");
        };
        assert!(form.contains("aria-invalid"), "{form}");
        assert!(
            form.contains("elementHandles") && form.contains("handle.evaluate"),
            "the read happens in the page, on the element itself: {form}",
        );
        assert!(form.contains("'value' in node"), "{form}");

        let Routed::Snippet(scoped) =
            route("browser_form_state", &json!({ "target": "e7" })).expect("routes")
        else {
            panic!("form_state is a snippet");
        };
        assert!(scoped.contains("aria-ref=e7"), "{scoped}");
    }

    /// The wait's expression is escaped into a JavaScript literal, so a quote
    /// or a newline in it cannot end the snippet early.
    #[test]
    fn an_expression_is_escaped_into_its_literal() {
        let Routed::Snippet(code) =
            route("browser_wait_for", &json!({ "expression": "a\"b\nc" })).expect("routes")
        else {
            panic!("an expression wait is a snippet");
        };
        assert!(code.contains(r#""a\"b\nc""#), "{code}");
        assert!(code.contains("waitForFunction"), "{code}");
    }
}
